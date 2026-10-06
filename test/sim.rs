//! A simulated virtio-net device (virtio 1.x PCI) that DMAs, shared by the
//! host tests: the common configuration (spec 4.1.4.3) with feature
//! selection and per-queue registers, the device configuration (MAC,
//! status), and split virtqueues processed the way spec 2.7 describes. A
//! notify on the transmit queue consumes every new available descriptor and
//! puts the frame on `wire` (looped back into the receive queue when
//! `loopback`); `inject` delivers a frame from the network into the next
//! posted receive buffer.
use crate::virtio::net::{Dma, Net, DMA_BYTES};
use crate::virtio::{Bus, Region};

pub const MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
/// Where the simulated IOMMU puts the region.
pub const DEVICE_BASE: u64 = 0x1_2345_0000;
pub const MULTIPLIER: u32 = 4;
/// What QEMU offers by default for a virtio-net-pci with no offloads
/// wanted: MAC, STATUS, VERSION_1 and a few the driver must leave alone
/// (CSUM 0, GUEST_CSUM 1, MRG_RXBUF 15, CTRL_VQ 17, RING_EVENT_IDX 29).
pub const OFFERED: u64 = 1 << 0 | 1 << 1 | 1 << 5 | 1 << 15 | 1 << 16 | 1 << 17 | 1 << 29 | 1 << 32;

#[derive(Clone, Default, Debug)]
pub struct Q {
    pub max: u16,
    pub size: u16,
    pub msix: u16,
    pub enable: u16,
    pub desc: u64,
    pub driver: u64,
    pub device: u64,
    /// The available idx the device has consumed up to.
    pub seen: u16,
    /// The used idx the device has published.
    pub used: u16,
}

pub struct Dev {
    /// The region's host memory: page aligned, leaked, reached only through
    /// this pointer (here and by `Net`), as the device's DMA would.
    base: *mut u8,
    pub status: u8,
    pub offered: u64,
    pub dfsel: u32,
    pub gfsel: u32,
    pub driver_features: u64,
    pub qsel: u16,
    pub q: [Q; 3],
    pub mac: [u8; 6],
    pub net_status: u16,
    pub generation: u8,
    /// Reads of device_status left before a reset shows 0.
    pub reset_delay: usize,
    /// Refuse FEATURES_OK.
    pub reject_features: bool,
    /// Every common-config write as (offset, value), and notifies as
    /// (0x1000 + offset, queue).
    pub ops: Vec<(u32, u32)>,
    pub wire: Vec<Vec<u8>>,
    pub loopback: bool,
    /// Frames the network sent that found no receive buffer.
    pub dropped: usize,
    pub delays: usize,
    /// Do not complete transmits (a stuck device).
    pub hold_tx: bool,
    /// Bump config_generation once during the next MAC read.
    pub change_during_read: bool,
    pub fail_io: bool,
}

impl Dev {
    pub fn new() -> Self {
        let q = Q { max: 256, ..Q::default() };
        Dev {
            base: {
                let v: &'static mut [u8] = Vec::leak(vec![0u8; DMA_BYTES + 4096]);
                let p = v.as_mut_ptr();
                unsafe { p.add(p.align_offset(4096)) }
            },
            status: 0, offered: OFFERED, dfsel: 0, gfsel: 0, driver_features: 0, qsel: 0,
            q: [q.clone(), q.clone(), q], mac: MAC, net_status: 1, generation: 0,
            reset_delay: 0, reject_features: false, ops: Vec::new(), wire: Vec::new(),
            loopback: false, dropped: 0, delays: 0, hold_tx: false, change_during_read: false,
            fail_io: false,
        }
    }

    /// The region as the driver sees it.
    pub fn dma(&mut self) -> Dma { Dma { host: self.base, device: DEVICE_BASE } }

    /// The host pointer behind device address `addr`, `len` bytes of it in
    /// the region.
    fn host(&self, addr: u64, len: usize) -> *mut u8 {
        assert!(addr >= DEVICE_BASE && addr + len as u64 <= DEVICE_BASE + DMA_BYTES as u64, "DMA outside the region: {addr:#x}");
        unsafe { self.base.add((addr - DEVICE_BASE) as usize) }
    }
    pub fn get(&self, addr: u64, len: usize) -> Vec<u8> {
        let p = self.host(addr, len);
        (0..len).map(|i| unsafe { std::ptr::read_volatile(p.add(i)) }).collect()
    }
    pub fn put(&mut self, addr: u64, bytes: &[u8]) {
        let p = self.host(addr, bytes.len());
        for (i, b) in bytes.iter().enumerate() { unsafe { std::ptr::write_volatile(p.add(i), *b) } }
    }
    pub fn r16(&self, addr: u64) -> u16 { u16::from_le_bytes(self.get(addr, 2).try_into().unwrap()) }
    pub fn r32(&self, addr: u64) -> u32 { u32::from_le_bytes(self.get(addr, 4).try_into().unwrap()) }
    pub fn r64(&self, addr: u64) -> u64 { u64::from_le_bytes(self.get(addr, 8).try_into().unwrap()) }
    fn w16(&mut self, addr: u64, v: u16) { self.put(addr, &v.to_le_bytes()) }
    fn w32(&mut self, addr: u64, v: u32) { self.put(addr, &v.to_le_bytes()) }

    pub fn running(&self) -> bool { self.status & 4 != 0 }

    fn reset(&mut self) {
        self.status = 0;
        self.driver_features = 0;
        for q in self.q.iter_mut() {
            *q = Q { max: q.max, ..Q::default() };
        }
    }

    /// Next available descriptor of queue `i` (id, addr, len, flags), taken.
    fn pop(&mut self, i: usize) -> Option<(u16, u64, u32, u16)> {
        let q = self.q[i].clone();
        if q.enable == 0 || !self.running() { return None; }
        let idx = self.r16(q.driver + 2);
        if idx == q.seen { return None; }
        let id = self.r16(q.driver + 4 + 2 * (q.seen % q.size) as u64);
        assert!(id < q.size, "available id {id} outside queue {i} of {}", q.size);
        let d = q.desc + 16 * id as u64;
        let (addr, len, flags) = (self.r64(d), self.r32(d + 8), self.r16(d + 12));
        self.q[i].seen = q.seen.wrapping_add(1);
        Some((id, addr, len, flags))
    }

    fn push_used(&mut self, i: usize, id: u16, len: u32) {
        let q = self.q[i].clone();
        let e = q.device + 4 + 8 * (q.used % q.size) as u64;
        self.w32(e, id as u32);
        self.w32(e + 4, len);
        self.q[i].used = q.used.wrapping_add(1);
        self.w16(q.device + 2, self.q[i].used);
    }

    /// The network sends `frame` to the device. False: no buffer posted.
    pub fn inject(&mut self, frame: &[u8]) -> bool {
        let Some((id, addr, len, flags)) = self.pop(0) else { self.dropped += 1; return false };
        assert_eq!(flags, 2, "receive descriptors are device-writable");
        assert!(len as usize >= 12 + frame.len());
        self.put(addr, &[0; 12]);
        self.put(addr + 12, frame);
        self.push_used(0, id, (12 + frame.len()) as u32);
        true
    }

    /// A raw used element on queue `i` (for malformed-device tests).
    pub fn push_raw(&mut self, i: usize, id: u16, len: u32) { self.push_used(i, id, len) }

    fn transmit(&mut self) {
        if self.hold_tx { return; }
        while let Some((id, addr, len, flags)) = self.pop(1) {
            assert_eq!(flags, 0, "transmit descriptors are device-readable, unchained");
            assert!(self.get(addr, 12).iter().all(|b| *b == 0), "no offloads: the header is zeros");
            let frame = self.get(addr + 12, len as usize - 12);
            self.push_used(1, id, 0);
            self.wire.push(frame.clone());
            if self.loopback { self.inject(&frame); }
        }
    }

    /// Complete transmits that `hold_tx` kept back.
    pub fn release_tx(&mut self) { self.hold_tx = false; self.transmit(); }

    pub fn queue(&self, i: usize) -> &Q { &self.q[i] }
}

impl Bus for Dev {
    type Error = &'static str;
    fn read(&mut self, region: Region, off: u32, width: u8) -> Result<u32, &'static str> {
        if self.fail_io { return Err("io"); }
        let (qmax, qenable) = { let q = &self.q[self.qsel.min(2) as usize]; (q.max, q.enable) };
        Ok(match (region, off, width) {
            (Region::Common, 4, 4) => (if self.dfsel == 0 { self.offered } else if self.dfsel == 1 { self.offered >> 32 } else { 0 }) as u32,
            (Region::Common, 18, 2) => 3,
            (Region::Common, 20, 1) => {
                if self.reset_delay > 0 && self.status == 0 { self.reset_delay -= 1; return Ok(1); }
                self.status as u32
            }
            (Region::Common, 21, 1) => self.generation as u32,
            (Region::Common, 24, 2) => if self.qsel < 3 { qmax as u32 } else { 0 },
            (Region::Common, 28, 2) => qenable as u32,
            (Region::Common, 30, 2) => self.qsel as u32 * 3,
            (Region::Device, o @ 0..=5, 1) => {
                if self.change_during_read && o == 3 { self.change_during_read = false; self.generation += 1; }
                self.mac[o as usize] as u32
            }
            (Region::Device, 6, 2) => self.net_status as u32,
            other => panic!("unexpected read {other:?}"),
        })
    }

    fn write(&mut self, region: Region, off: u32, width: u8, v: u32) -> Result<(), &'static str> {
        if self.fail_io { return Err("io"); }
        match region {
            Region::Notify => {
                assert_eq!(width, 2);
                self.ops.push((0x1000 + off, v));
                let qi = v as usize;
                assert_eq!(off, qi as u32 * 3 * MULTIPLIER, "notify at the queue's address");
                if qi == 1 { self.transmit(); }
                return Ok(());
            }
            Region::Common => {}
            r => panic!("unexpected write to {r:?}"),
        }
        self.ops.push((off, v));
        let qs = self.qsel as usize;
        match (off, width) {
            (0, 4) => self.dfsel = v,
            (8, 4) => self.gfsel = v,
            (12, 4) => {
                let shift = 32 * self.gfsel as u64;
                self.driver_features = (self.driver_features & !(0xffff_ffffu64 << shift)) | (v as u64) << shift;
            }
            (20, 1) => {
                let v = v as u8;
                if v == 0 { self.reset(); return Ok(()); }
                if v & 8 != 0 && self.status & 8 == 0 {
                    assert_eq!(self.driver_features & !self.offered, 0, "only offered features accepted");
                    if self.reject_features { self.status = v & !8; return Ok(()); }
                }
                self.status = v;
            }
            (22, 2) => self.qsel = v as u16,
            (24, 2) => {
                assert!(v as u16 <= self.q[qs].max && (v as u16).is_power_of_two());
                self.q[qs].size = v as u16;
            }
            (26, 2) => self.q[qs].msix = v as u16,
            (28, 2) => { assert_eq!(v, 1); self.q[qs].enable = 1; }
            (32, 4) => self.q[qs].desc = (self.q[qs].desc & !0xffff_ffff) | v as u64,
            (36, 4) => self.q[qs].desc = (self.q[qs].desc & 0xffff_ffff) | (v as u64) << 32,
            (40, 4) => self.q[qs].driver = (self.q[qs].driver & !0xffff_ffff) | v as u64,
            (44, 4) => self.q[qs].driver = (self.q[qs].driver & 0xffff_ffff) | (v as u64) << 32,
            (48, 4) => self.q[qs].device = (self.q[qs].device & !0xffff_ffff) | v as u64,
            (52, 4) => self.q[qs].device = (self.q[qs].device & 0xffff_ffff) | (v as u64) << 32,
            other => panic!("unexpected common write {other:?} = {v:#x}"),
        }
        Ok(())
    }

    fn delay_us(&mut self, _us: usize) { self.delays += 1; }
}

/// A device and an idle `Net` over its region.
pub fn setup() -> (Dev, Net) {
    let mut dev = Dev::new();
    let dma = dev.dma();
    // SAFETY: the region is leaked by `Dev::new`, so it outlives both.
    let net = unsafe { Net::new(dma) };
    (dev, net)
}

/// Started queues.
pub fn started() -> (Dev, Net) {
    let (mut dev, mut net) = setup();
    net.start(&mut dev, MULTIPLIER).unwrap();
    (dev, net)
}

/// A broadcast frame of `len` bytes with a marker byte.
pub fn frame_to(destination: [u8; 6], len: usize, mark: u8) -> Vec<u8> {
    let mut f = vec![mark; len];
    f[0..6].copy_from_slice(&destination);
    f[6..12].copy_from_slice(&[0x02, 0, 0, 0, 0, 9]);
    f[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
    f
}
