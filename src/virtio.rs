//! virtio 1.x over PCI (the modern transport), from the OASIS virtio
//! specification: section 4.1 (PCI capabilities and the common
//! configuration structure), 3.1 (device initialization), 2.1 (device
//! status) and 6 (reserved feature bits). Section numbers in comments
//! ("spec 4.1.4.3") point into virtio 1.2.
//!
//! Independent of UEFI, like stormnic-ixgbe's `hardware`, so every sequence
//! can be tested against a simulated device: the caller finds the regions
//! with `capabilities` (config space reads), then reaches them through
//! `Bus`, and supplies firmware Stall for delays. `net` (the virtio-net
//! queues) and `snp` (the SNP state machine) build on this.

/// PCI vendor of every virtio device.
pub const VENDOR: u16 = 0x1af4;

/// The structures the device's vendor capabilities point at (spec 4.1.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Region {
    Common,
    Notify,
    /// Not used: the driver polls.
    #[allow(dead_code)]
    Isr,
    Device,
}

/// Where a structure is: BAR register index (0-5), offset in it, length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    pub bar: u8,
    pub offset: u32,
    pub length: u32,
}

/// The device's virtio capabilities: the first usable one of each type, as
/// spec 4.1.4.1 asks of a driver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps {
    pub common: Window,
    pub notify: Window,
    /// `notify_off_multiplier` (spec 4.1.4.4).
    pub notify_multiplier: u32,
    pub isr: Option<Window>,
    pub device: Window,
}

impl Caps {
    pub fn window(&self, r: Region) -> Option<Window> {
        match r {
            Region::Common => Some(self.common),
            Region::Notify => Some(self.notify),
            Region::Isr => self.isr,
            Region::Device => Some(self.device),
        }
    }
}

/// Why a function has no usable virtio 1.x capabilities.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapError<E> {
    Io(E),
    /// Status register bit 4 clear: no capability list at all.
    NoList,
    /// No capability of this `cfg_type` (1 common, 2 notify, 4 device), or
    /// one too short: a legacy-only device.
    Missing(u8),
    /// The list did not end within 48 entries (the most config space holds).
    Loop,
}

// `cfg_type` values (spec 4.1.4).
const COMMON_CFG: u8 = 1;
const NOTIFY_CFG: u8 = 2;
const ISR_CFG: u8 = 3;
const DEVICE_CFG: u8 = 4;
/// PCI capability ID: vendor specific.
const CAP_VENDOR: u8 = 0x09;
/// Config space: status register (upper half of dword 4), capability pointer.
const STATUS_CAP_LIST: u32 = 1 << (16 + 4);
const CAP_POINTER: u32 = 0x34;

/// Walk the PCI capability list with `cfg` (a dword read of config space at
/// a 4-aligned offset) and collect the virtio structures (spec 4.1.4).
/// Capabilities naming a reserved BAR (above 5) are ignored, as spec
/// 4.1.4.1 requires.
pub fn capabilities<E>(mut cfg: impl FnMut(u32) -> Result<u32, E>) -> Result<Caps, CapError<E>> {
    if cfg(0x04).map_err(CapError::Io)? & STATUS_CAP_LIST == 0 {
        return Err(CapError::NoList);
    }
    let mut ptr = cfg(CAP_POINTER).map_err(CapError::Io)? & 0xfc;
    let (mut common, mut notify, mut isr, mut device) = (None, None, None, None);
    let mut multiplier = 0;
    let mut seen = 0;
    while ptr != 0 {
        seen += 1;
        if seen > 48 {
            return Err(CapError::Loop);
        }
        let head = cfg(ptr).map_err(CapError::Io)?;
        let [id, next, len, kind] = head.to_le_bytes();
        if id == CAP_VENDOR && len >= 16 {
            let bar = cfg(ptr + 4).map_err(CapError::Io)? as u8;
            let w = Window {
                bar,
                offset: cfg(ptr + 8).map_err(CapError::Io)?,
                length: cfg(ptr + 12).map_err(CapError::Io)?,
            };
            if bar <= 5 {
                match kind {
                    COMMON_CFG if common.is_none() => common = Some(w),
                    NOTIFY_CFG if notify.is_none() && len >= 20 => {
                        multiplier = cfg(ptr + 16).map_err(CapError::Io)?;
                        notify = Some(w);
                    }
                    ISR_CFG if isr.is_none() => isr = Some(w),
                    DEVICE_CFG if device.is_none() => device = Some(w),
                    _ => {}
                }
            }
        }
        ptr = u32::from(next) & 0xfc;
    }
    Ok(Caps {
        common: common.ok_or(CapError::Missing(COMMON_CFG))?,
        notify: notify.ok_or(CapError::Missing(NOTIFY_CFG))?,
        notify_multiplier: multiplier,
        isr,
        device: device.ok_or(CapError::Missing(DEVICE_CFG))?,
    })
}

/// Access to the device's structures: a `width`-byte (1, 2 or 4) read or
/// write at `offset` in `region`, and a delay.
pub trait Bus {
    type Error;
    fn read(&mut self, region: Region, offset: u32, width: u8) -> Result<u32, Self::Error>;
    fn write(&mut self, region: Region, offset: u32, width: u8, value: u32) -> Result<(), Self::Error>;
    fn delay_us(&mut self, micros: usize);
}

#[derive(Debug, PartialEq, Eq)]
pub enum Error<E> {
    Io(E),
    /// device_status read 0xff: the function is gone.
    Removed,
    /// device_status not 0 within 1 s of a reset (spec 4.1.4.3.2).
    ResetTimeout { status: u8 },
    /// The device does not offer VIRTIO_F_VERSION_1: not a virtio 1.x device.
    NotModern { offered: u64 },
    /// No VIRTIO_NET_F_MAC: the device has no address to give.
    NoMac { offered: u64 },
    /// FEATURES_OK did not stay set (spec 3.1.1 step 6).
    FeaturesRejected { features: u64, status: u8 },
    /// The device reports DEVICE_NEEDS_RESET or FAILED (spec 2.1).
    DeviceStatus { status: u8 },
    /// queue_size 0: the queue does not exist (spec 4.1.4.3).
    NoQueue { queue: u16 },
    /// The device returned a used element naming a descriptor that is not
    /// outstanding.
    BadUsed { queue: u16, id: u32 },
    /// A frame outside 14..=1518 bytes to send, or a received frame longer
    /// than the caller's buffer (its length; it stays queued).
    FrameLength { len: usize },
}

// The common configuration structure (spec 4.1.4.3), byte offsets.
pub const DEVICE_FEATURE_SELECT: u32 = 0;
pub const DEVICE_FEATURE: u32 = 4;
pub const DRIVER_FEATURE_SELECT: u32 = 8;
pub const DRIVER_FEATURE: u32 = 12;
pub const NUM_QUEUES: u32 = 18;
pub const DEVICE_STATUS: u32 = 20;
pub const CONFIG_GENERATION: u32 = 21;
pub const QUEUE_SELECT: u32 = 22;
pub const QUEUE_SIZE: u32 = 24;
pub const QUEUE_MSIX_VECTOR: u32 = 26;
pub const QUEUE_ENABLE: u32 = 28;
pub const QUEUE_NOTIFY_OFF: u32 = 30;
pub const QUEUE_DESC: u32 = 32;
pub const QUEUE_DRIVER: u32 = 40;
pub const QUEUE_DEVICE: u32 = 48;
/// VIRTIO_MSI_NO_VECTOR: this driver polls.
pub const NO_VECTOR: u32 = 0xffff;

// Device status bits (spec 2.1).
pub const ACKNOWLEDGE: u8 = 1;
pub const DRIVER: u8 = 2;
pub const DRIVER_OK: u8 = 4;
pub const FEATURES_OK: u8 = 8;
pub const NEEDS_RESET: u8 = 64;
pub const FAILED: u8 = 128;

// Feature bits (spec 5.1.3 and 6).
pub const F_NET_MAC: u64 = 1 << 5;
pub const F_NET_STATUS: u64 = 1 << 16;
pub const F_VERSION_1: u64 = 1 << 32;
/// The device reaches memory through the platform's IOMMU (spec 6): the
/// driver's addresses come from PciIo Map, so it is accepted when offered.
pub const F_ACCESS_PLATFORM: u64 = 1 << 33;
/// The device needs the platform's memory barriers: the driver uses real
/// fences (SeqCst), so it is accepted when offered.
pub const F_ORDER_PLATFORM: u64 = 1 << 36;
/// What the driver accepts. No mergeable buffers, offloads or control queue:
/// one RX and one TX queue with 2 KiB buffers.
pub const ACCEPTED: u64 = F_NET_MAC | F_NET_STATUS | F_VERSION_1 | F_ACCESS_PLATFORM | F_ORDER_PLATFORM;

// virtio-net device configuration (spec 5.1.4).
const NET_MAC: u32 = 0;
const NET_STATUS: u32 = 6;
const LINK_UP: u32 = 1;

pub type R<T, E> = Result<T, Error<E>>;

pub fn read8<B: Bus>(b: &mut B, r: Region, off: u32) -> R<u8, B::Error> {
    b.read(r, off, 1).map(|v| v as u8).map_err(Error::Io)
}
pub fn read16<B: Bus>(b: &mut B, r: Region, off: u32) -> R<u16, B::Error> {
    b.read(r, off, 2).map(|v| v as u16).map_err(Error::Io)
}
pub fn read32<B: Bus>(b: &mut B, r: Region, off: u32) -> R<u32, B::Error> {
    b.read(r, off, 4).map_err(Error::Io)
}
pub fn write8<B: Bus>(b: &mut B, r: Region, off: u32, v: u8) -> R<(), B::Error> {
    b.write(r, off, 1, v.into()).map_err(Error::Io)
}
pub fn write16<B: Bus>(b: &mut B, r: Region, off: u32, v: u16) -> R<(), B::Error> {
    b.write(r, off, 2, v.into()).map_err(Error::Io)
}
pub fn write32<B: Bus>(b: &mut B, r: Region, off: u32, v: u32) -> R<(), B::Error> {
    b.write(r, off, 4, v).map_err(Error::Io)
}
/// A 64-bit field of the common configuration as two 32-bit writes, low
/// half first (spec 4.1.3.1 allows a driver to split them).
pub fn write64<B: Bus>(b: &mut B, off: u32, v: u64) -> R<(), B::Error> {
    write32(b, Region::Common, off, v as u32)?;
    write32(b, Region::Common, off + 4, (v >> 32) as u32)
}

pub fn status<B: Bus>(b: &mut B) -> R<u8, B::Error> {
    let s = read8(b, Region::Common, DEVICE_STATUS)?;
    if s == 0xff {
        return Err(Error::Removed);
    }
    Ok(s)
}

fn set_status<B: Bus>(b: &mut B, s: u8) -> R<(), B::Error> {
    write8(b, Region::Common, DEVICE_STATUS, s)
}

/// Reset: write 0 to device_status and wait (1 ms steps, up to 1 s) for it
/// to read back 0 (spec 4.1.4.3.2). Afterwards the device uses no queue
/// memory, so this is also how the driver stops DMA.
pub fn reset<B: Bus>(b: &mut B) -> R<(), B::Error> {
    set_status(b, 0)?;
    for elapsed in 0..=1000 {
        let s = status(b)?;
        if s == 0 {
            return Ok(());
        }
        if elapsed == 1000 {
            return Err(Error::ResetTimeout { status: s });
        }
        b.delay_us(1000);
    }
    unreachable!()
}

/// Spec 3.1.1 steps 1-6: reset, ACKNOWLEDGE, DRIVER, read the offered
/// features, accept `ACCEPTED` of them (VIRTIO_F_VERSION_1 and
/// VIRTIO_NET_F_MAC required), FEATURES_OK and check it stayed. Returns the
/// negotiated features. On Err the device is left as it failed; the caller
/// resets it.
pub fn negotiate<B: Bus>(b: &mut B) -> R<u64, B::Error> {
    reset(b)?;
    set_status(b, ACKNOWLEDGE)?;
    set_status(b, ACKNOWLEDGE | DRIVER)?;
    write32(b, Region::Common, DEVICE_FEATURE_SELECT, 0)?;
    let low = read32(b, Region::Common, DEVICE_FEATURE)?;
    write32(b, Region::Common, DEVICE_FEATURE_SELECT, 1)?;
    let high = read32(b, Region::Common, DEVICE_FEATURE)?;
    let offered = u64::from(high) << 32 | u64::from(low);
    if offered & F_VERSION_1 == 0 {
        set_status(b, ACKNOWLEDGE | DRIVER | FAILED)?;
        return Err(Error::NotModern { offered });
    }
    if offered & F_NET_MAC == 0 {
        set_status(b, ACKNOWLEDGE | DRIVER | FAILED)?;
        return Err(Error::NoMac { offered });
    }
    let features = offered & ACCEPTED;
    write32(b, Region::Common, DRIVER_FEATURE_SELECT, 0)?;
    write32(b, Region::Common, DRIVER_FEATURE, features as u32)?;
    write32(b, Region::Common, DRIVER_FEATURE_SELECT, 1)?;
    write32(b, Region::Common, DRIVER_FEATURE, (features >> 32) as u32)?;
    set_status(b, ACKNOWLEDGE | DRIVER | FEATURES_OK)?;
    let s = status(b)?;
    if s & FEATURES_OK == 0 {
        return Err(Error::FeaturesRejected { features, status: s });
    }
    Ok(features)
}

/// Mark the device live (spec 3.1.1 step 8) and check it did not fail.
pub fn driver_ok<B: Bus>(b: &mut B) -> R<(), B::Error> {
    set_status(b, ACKNOWLEDGE | DRIVER | FEATURES_OK | DRIVER_OK)?;
    let s = status(b)?;
    if s & (NEEDS_RESET | FAILED) != 0 {
        return Err(Error::DeviceStatus { status: s });
    }
    Ok(())
}

/// The MAC address in the device configuration, read as one consistent
/// snapshot: config_generation the same before and after (spec 4.1.4.3.1).
pub fn mac<B: Bus>(b: &mut B) -> R<[u8; 6], B::Error> {
    let mut m = [0u8; 6];
    for _ in 0..4 {
        let before = read8(b, Region::Common, CONFIG_GENERATION)?;
        for (i, byte) in m.iter_mut().enumerate() {
            *byte = read8(b, Region::Device, NET_MAC + i as u32)?;
        }
        if read8(b, Region::Common, CONFIG_GENERATION)? == before {
            break;
        }
    }
    Ok(m)
}

/// Link state: VIRTIO_NET_S_LINK_UP when VIRTIO_NET_F_STATUS was
/// negotiated; without it the link is always up (spec 5.1.4.2).
pub fn link_up<B: Bus>(b: &mut B, features: u64) -> R<bool, B::Error> {
    if features & F_NET_STATUS == 0 {
        return Ok(true);
    }
    Ok(u32::from(read16(b, Region::Device, NET_STATUS)?) & LINK_UP != 0)
}

/// What Start learns before any queue runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
    pub features: u64,
    pub mac: [u8; 6],
    pub link: bool,
    pub queues: u16,
}

/// Negotiate, read the MAC, link and queue count, and reset again: the
/// device is left idle until `net::Net::start`.
pub fn identify<B: Bus>(b: &mut B) -> R<Identity, B::Error> {
    let r = read_identity(b);
    match (r, reset(b)) {
        (Ok(id), Ok(())) => Ok(id),
        (Err(e), _) | (Ok(_), Err(e)) => Err(e),
    }
}

fn read_identity<B: Bus>(b: &mut B) -> R<Identity, B::Error> {
    let features = negotiate(b)?;
    let mac = mac(b)?;
    let link = link_up(b, features)?;
    let queues = read16(b, Region::Common, NUM_QUEUES)?;
    Ok(Identity { features, mac, link, queues })
}

#[path = "queue.rs"]
pub mod queue;
#[path = "net.rs"]
pub mod net;
#[path = "snp_core.rs"]
pub mod snp;
