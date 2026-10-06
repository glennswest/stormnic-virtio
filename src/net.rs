//! The virtio-net queues (spec 5.1): receive queue 0 and transmit queue 1,
//! one 2 KiB driver-owned buffer per descriptor, polled.
//!
//! Every buffer starts with the 12-byte `virtio_net_hdr` (spec 5.1.6: with
//! VIRTIO_F_VERSION_1 it always includes `num_buffers`). With no offload
//! features negotiated, a transmitted header is all zeros and a received one
//! is ignored, and without mergeable buffers each frame arrives in one
//! buffer (spec 5.1.6.3.1), which 2 KiB holds.
//!
//! Region layout (`DMA_BYTES`, one PciIo common buffer): the receive
//! queue's slot at 0, the transmit queue's at 2048 (`queue.rs`), then from
//! 4096 the 32 receive buffers and the 32 transmit buffers. Frames are
//! copied in and out of them, so no caller memory is ever mapped for DMA.
//!
//! `start` is the whole of spec 3.1.1 (reset, negotiate, queues, DRIVER_OK)
//! and `stop` is a reset, after which the device touches no queue memory:
//! the SNP's Initialize and Shutdown map onto them directly.

use super::queue::{self, Queue, DESC_WRITE};
use super::{Bus, Error, R};
use core::ptr;

pub const RX: u16 = 0;
pub const TX: u16 = 1;
pub const BUF_SIZE: usize = 2048;
/// `virtio_net_hdr` with VIRTIO_F_VERSION_1.
pub const HEADER: usize = 12;
const RX_SLOT: usize = 0;
const TX_SLOT: usize = queue::SLOT;
const RX_BUFS: usize = 2 * queue::SLOT;
const TX_BUFS: usize = RX_BUFS + queue::MAX as usize * BUF_SIZE;
pub const DMA_BYTES: usize = TX_BUFS + queue::MAX as usize * BUF_SIZE;
pub const DMA_PAGES: usize = DMA_BYTES.div_ceil(4096);

/// Largest frame without CRC: 1514 plus a 4-byte VLAN tag.
pub const MAX_FRAME: usize = 1518;
/// Ethernet header: the smallest frame the driver will send.
pub const MIN_FRAME: usize = 14;
/// Short frames are padded to this on transmit (the 64-byte minimum less
/// the CRC the backend adds or never needs).
const PAD_TO: usize = 60;

/// The DMA region: `DMA_BYTES` at `host`, which the device reaches at `device`.
#[derive(Clone, Copy, Debug)]
pub struct Dma {
    pub host: *mut u8,
    pub device: u64,
}

/// A received frame's summary, for the console.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    pub len: usize,
    pub destination: [u8; 6],
    pub source: [u8; 6],
    pub ethertype: u16,
}

pub struct Net {
    dma: Dma,
    rx: Queue,
    tx: Queue,
    /// The negotiated features of the last `start`.
    pub features: u64,
    /// Transmit descriptors not given to the device, as a stack.
    free: [u16; queue::MAX as usize],
    nfree: usize,
    /// Queues running (between `start` and `stop`).
    pub running: bool,
}

impl Net {
    /// # Safety
    /// `dma.host` must point to `DMA_BYTES` of memory that stays valid, and
    /// is used only through this value, until it is dropped; the device must
    /// reach it at `dma.device`.
    pub unsafe fn new(dma: Dma) -> Self {
        // SAFETY: both slots are inside the region (caller).
        let (rx, tx) = unsafe {
            (Queue::new(RX, dma.host.add(RX_SLOT), dma.device + RX_SLOT as u64),
             Queue::new(TX, dma.host.add(TX_SLOT), dma.device + TX_SLOT as u64))
        };
        Net { dma, rx, tx, features: 0, free: [0; queue::MAX as usize], nfree: 0, running: false }
    }

    fn rx_buf(&self, id: u16) -> *mut u8 {
        // SAFETY (for callers): buffer id < MAX inside the region.
        unsafe { self.dma.host.add(RX_BUFS + id as usize * BUF_SIZE) }
    }
    fn tx_buf(&self, id: u16) -> *mut u8 {
        // SAFETY (for callers): as rx_buf.
        unsafe { self.dma.host.add(TX_BUFS + id as usize * BUF_SIZE) }
    }

    /// Queue sizes in use: (receive, transmit).
    pub fn sizes(&self) -> (u16, u16) { (self.rx.size, self.tx.size) }

    /// Spec 3.1.1 in full: reset, negotiate, both queues, every receive
    /// buffer posted, DRIVER_OK, and the receive queue notified. On Err the
    /// device is reset again, so it is not left half-started.
    pub fn start<B: Bus>(&mut self, b: &mut B, multiplier: u32) -> R<(), B::Error> {
        self.running = false;
        let r = self.bring_up(b, multiplier);
        if r.is_err() {
            let _ = super::reset(b);
        } else {
            self.running = true;
        }
        r
    }

    fn bring_up<B: Bus>(&mut self, b: &mut B, multiplier: u32) -> R<(), B::Error> {
        self.features = super::negotiate(b)?;
        for q in [&mut self.rx, &mut self.tx] {
            if !q.setup(b, multiplier)? {
                return Err(Error::NoQueue { queue: q.index });
            }
        }
        for id in 0..self.rx.size {
            let addr = self.dma.device + (RX_BUFS + id as usize * BUF_SIZE) as u64;
            self.rx.set_desc(id, addr, BUF_SIZE as u32, DESC_WRITE);
            self.rx.stage(id);
        }
        self.rx.publish();
        self.nfree = self.tx.size as usize;
        for (i, slot) in self.free.iter_mut().enumerate().take(self.nfree) {
            *slot = i as u16;
        }
        super::driver_ok(b)?;
        self.rx.notify(b)
    }

    /// Stop DMA: let queued frames go (up to 100 ms), then reset the device.
    /// Returns the frames left unsent.
    pub fn stop<B: Bus>(&mut self, b: &mut B) -> R<usize, B::Error> {
        let mut left = 0;
        if self.running {
            left = self.tx_pending()?;
            for _ in 0..100 {
                if left == 0 { break; }
                b.delay_us(1000);
                left = self.tx_pending()?;
            }
        }
        self.running = false;
        super::reset(b)?;
        Ok(left)
    }

    pub fn link<B: Bus>(&self, b: &mut B) -> R<bool, B::Error> {
        super::link_up(b, self.features)
    }

    /// Queue one frame (header included, no CRC). Ok(false): every transmit
    /// descriptor is with the device; `reclaim` first.
    pub fn transmit<B: Bus>(&mut self, b: &mut B, frame: &[u8]) -> R<bool, B::Error> {
        if frame.len() < MIN_FRAME || frame.len() > MAX_FRAME {
            return Err(Error::FrameLength { len: frame.len() });
        }
        self.reclaim()?;
        if self.nfree == 0 {
            return Ok(false);
        }
        self.nfree -= 1;
        let id = self.free[self.nfree];
        let len = frame.len().max(PAD_TO);
        let buf = self.tx_buf(id);
        // SAFETY: the buffer is BUF_SIZE >= HEADER + MAX_FRAME bytes in the
        // region and not with the device (its id was free).
        unsafe {
            ptr::write_bytes(buf, 0, HEADER + len);
            ptr::copy_nonoverlapping(frame.as_ptr(), buf.add(HEADER), frame.len());
        }
        let addr = self.dma.device + (TX_BUFS + id as usize * BUF_SIZE) as u64;
        self.tx.set_desc(id, addr, (HEADER + len) as u32, 0);
        self.tx.stage(id);
        self.tx.publish();
        self.tx.notify(b)?;
        Ok(true)
    }

    /// Frames the device has sent since the last call; their descriptors
    /// are free again.
    pub fn reclaim<E>(&mut self) -> R<usize, E> {
        let mut n = 0;
        while let Some((id, _)) = self.tx.peek_used() {
            let outstanding = id < u32::from(self.tx.size) && !self.free[..self.nfree].contains(&(id as u16));
            if !outstanding {
                return Err(Error::BadUsed { queue: TX, id });
            }
            self.tx.take_used();
            self.free[self.nfree] = id as u16;
            self.nfree += 1;
            n += 1;
        }
        Ok(n)
    }

    /// Frames queued and not yet sent.
    pub fn tx_pending<E>(&mut self) -> R<usize, E> {
        self.reclaim()?;
        Ok(self.tx.size as usize - self.nfree)
    }

    /// Give the buffer of the next used receive element back to the device.
    fn recycle<B: Bus>(&mut self, b: &mut B, id: u16) -> R<(), B::Error> {
        self.rx.take_used();
        self.rx.stage(id);
        self.rx.publish();
        self.rx.notify(b)
    }

    /// Descriptor and frame length of the next good received frame, if any.
    /// Elements too short to hold a header and an Ethernet header, or longer
    /// than a buffer, are dropped on the way.
    fn next<B: Bus>(&mut self, b: &mut B) -> R<Option<(u16, usize)>, B::Error> {
        while let Some((id, len)) = self.rx.peek_used() {
            if id >= u32::from(self.rx.size) {
                return Err(Error::BadUsed { queue: RX, id });
            }
            let len = len as usize;
            if (HEADER + MIN_FRAME..=BUF_SIZE).contains(&len) {
                return Ok(Some((id as u16, len - HEADER)));
            }
            self.recycle(b, id as u16)?;
        }
        Ok(None)
    }

    /// Length and destination address of the next good received frame,
    /// left queued.
    pub fn peek<B: Bus>(&mut self, b: &mut B) -> R<Option<(usize, [u8; 6])>, B::Error> {
        let Some((id, len)) = self.next(b)? else { return Ok(None) };
        let mut destination = [0u8; 6];
        // SAFETY: the device wrote at least HEADER + MIN_FRAME bytes and
        // published the element; the buffer is the driver's until `recycle`.
        unsafe { ptr::copy_nonoverlapping(self.rx_buf(id).add(HEADER), destination.as_mut_ptr(), 6) };
        Ok(Some((len, destination)))
    }

    /// Drop the frame `peek` returned: its buffer goes back to the device.
    pub fn skip<B: Bus>(&mut self, b: &mut B) -> R<(), B::Error> {
        if let Some((id, _)) = self.next(b)? { self.recycle(b, id)?; }
        Ok(())
    }

    /// Copy the next good frame into `out` and give its buffer back.
    /// Ok(None): nothing received. A frame longer than `out` stays queued
    /// and fails with `FrameLength` (its length), so the caller can retry.
    pub fn receive<B: Bus>(&mut self, b: &mut B, out: &mut [u8]) -> R<Option<usize>, B::Error> {
        let Some((id, len)) = self.next(b)? else { return Ok(None) };
        if len > out.len() { return Err(Error::FrameLength { len }); }
        // SAFETY: as `peek`; HEADER + len <= BUF_SIZE (`next`).
        unsafe { ptr::copy_nonoverlapping(self.rx_buf(id).add(HEADER), out.as_mut_ptr(), len) };
        self.recycle(b, id)?;
        Ok(Some(len))
    }
}

/// Summary of an Ethernet frame for the console.
pub fn frame(bytes: &[u8]) -> Frame {
    let mut destination = [0; 6];
    let mut source = [0; 6];
    destination.copy_from_slice(&bytes[0..6]);
    source.copy_from_slice(&bytes[6..12]);
    Frame { len: bytes.len(), destination, source, ethertype: u16::from_be_bytes([bytes[12], bytes[13]]) }
}

/// The DMA check frame: broadcast, from `mac`, EtherType 0x88B5 (IEEE
/// 802 local experimental), a text payload, 60 bytes.
pub fn check_frame(mac: [u8; 6]) -> [u8; 60] {
    let mut f = [0u8; 60];
    f[0..6].copy_from_slice(&[0xff; 6]);
    f[6..12].copy_from_slice(&mac);
    f[12..14].copy_from_slice(&0x88b5u16.to_be_bytes());
    let text = b"stormnic-virtio DMA check";
    f[14..14 + text.len()].copy_from_slice(text);
    f
}

/// Send the check frame and wait up to 100 ms for the device to return its
/// descriptor: the device read the transmit queue and wrote the used ring,
/// so DMA works both ways. Ok(Some(ms)) when it came back.
pub fn check<B: Bus>(b: &mut B, net: &mut Net, mac: [u8; 6]) -> R<Option<usize>, B::Error> {
    if !net.transmit(b, &check_frame(mac))? {
        return Ok(None);
    }
    for ms in 0..=100 {
        if net.tx_pending()? == 0 { return Ok(Some(ms)); }
        b.delay_us(1000);
    }
    Ok(None)
}
