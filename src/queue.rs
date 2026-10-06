//! One split virtqueue in the driver's DMA region (spec 2.7): the
//! descriptor table, the available (driver) ring and the used (device) ring,
//! each at a fixed place in a 2 KiB slot:
//!
//! | offset | structure | size for `MAX` = 32 | alignment (spec 2.7) |
//! |---|---|---|---|
//! | 0 | descriptor table | 16 x 32 = 512 | 16 |
//! | 512 | available ring: flags, idx, ring[32], used_event | 70 | 2 |
//! | 1024 | used ring: flags, idx, ring[32] of (id, len), avail_event | 262 | 4 |
//!
//! Everything here is plain memory the device reads and writes by DMA, so
//! every access is volatile, and the fences (SeqCst: a real barrier on every
//! platform, which VIRTIO_F_ORDER_PLATFORM asks for) order the driver's
//! writes of descriptors before the index that publishes them, and the
//! device's index before the elements read behind it (spec 2.7.13, 2.7.14).

use super::{write16, Bus, Region, R, QUEUE_DESC, QUEUE_DEVICE, QUEUE_DRIVER, QUEUE_ENABLE, QUEUE_MSIX_VECTOR,
    QUEUE_NOTIFY_OFF, QUEUE_SELECT, QUEUE_SIZE, NO_VECTOR};
use core::ptr;
use core::sync::atomic::{fence, Ordering};

/// Most entries a queue is given; the device may offer more (QEMU: 256).
pub const MAX: u16 = 32;
/// Bytes of the region one queue uses.
pub const SLOT: usize = 2048;
const AVAIL: usize = 512;
const USED: usize = 1024;

/// Descriptor flags (spec 2.7.5).
pub const DESC_WRITE: u16 = 2;
/// Available ring flag: no used-buffer notifications wanted (spec 2.7.7);
/// this driver polls.
pub const AVAIL_NO_INTERRUPT: u16 = 1;

/// The queue size to use when the device offers `max`: at most `MAX`, and a
/// power of two (spec 2.7: a split queue's size always is; a device that
/// offers otherwise gets the largest power of two below).
pub fn size_for(max: u16) -> u16 {
    let n = max.min(MAX);
    if n == 0 { 0 } else { 1 << (15 - n.leading_zeros()) }
}

pub struct Queue {
    pub index: u16,
    pub size: u16,
    /// Host address of the queue's slot in the region.
    host: *mut u8,
    /// The address the device uses for the slot.
    device: u64,
    /// Offset of this queue's notification address in the notify region
    /// (queue_notify_off x notify_off_multiplier, spec 4.1.4.4).
    pub notify: u32,
    /// The available ring's idx as last published.
    avail_idx: u16,
    /// The used ring's idx up to which elements have been taken.
    pub last_used: u16,
}

impl Queue {
    /// # Safety
    /// `host` points to `SLOT` bytes of the DMA region, used only through
    /// this queue, which the device reaches at `device`.
    pub unsafe fn new(index: u16, host: *mut u8, device: u64) -> Self {
        Queue { index, size: 0, host, device, notify: 0, avail_idx: 0, last_used: 0 }
    }

    fn at(&self, offset: usize) -> *mut u8 {
        // SAFETY (for callers' volatile accesses): every offset used is
        // below SLOT (see the table above), inside the slot (`new`).
        unsafe { self.host.add(offset) }
    }

    /// Clear the slot: no descriptors, both rings at index 0, and the
    /// available ring asking for no interrupts.
    pub fn clear(&mut self) {
        // SAFETY: the slot is ours and the device is reset (not using it).
        unsafe { ptr::write_bytes(self.host, 0, SLOT) };
        // SAFETY: the available ring's flags, 2-byte aligned in the slot.
        unsafe { ptr::write_volatile(self.at(AVAIL) as *mut u16, AVAIL_NO_INTERRUPT) };
        self.avail_idx = 0;
        self.last_used = 0;
    }

    /// Program the queue into the device (spec 4.1.5.1.3): select it, read
    /// its maximum size and write ours, no MSI-X vector, the three
    /// addresses, read its notification offset, enable it. The slot is
    /// cleared first. Ok(false): the device has no such queue.
    pub fn setup<B: Bus>(&mut self, b: &mut B, multiplier: u32) -> R<bool, B::Error> {
        write16(b, Region::Common, QUEUE_SELECT, self.index)?;
        let max = super::read16(b, Region::Common, QUEUE_SIZE)?;
        self.size = size_for(max);
        if self.size == 0 {
            return Ok(false);
        }
        self.clear();
        write16(b, Region::Common, QUEUE_SIZE, self.size)?;
        write16(b, Region::Common, QUEUE_MSIX_VECTOR, NO_VECTOR as u16)?;
        super::write64(b, QUEUE_DESC, self.device)?;
        super::write64(b, QUEUE_DRIVER, self.device + AVAIL as u64)?;
        super::write64(b, QUEUE_DEVICE, self.device + USED as u64)?;
        let off = super::read16(b, Region::Common, QUEUE_NOTIFY_OFF)?;
        self.notify = u32::from(off).wrapping_mul(multiplier);
        write16(b, Region::Common, QUEUE_ENABLE, 1)?;
        Ok(true)
    }

    /// Fill descriptor `i`: buffer at device address `addr`, `len` bytes,
    /// `flags` (no chaining: one buffer per descriptor).
    pub fn set_desc(&self, i: u16, addr: u64, len: u32, flags: u16) {
        let d = self.at(16 * i as usize);
        // SAFETY: descriptor i < size <= MAX in the table, 16-byte aligned.
        unsafe {
            ptr::write_volatile(d as *mut u64, addr);
            ptr::write_volatile(d.add(8) as *mut u32, len);
            ptr::write_volatile(d.add(12) as *mut u16, flags);
            ptr::write_volatile(d.add(14) as *mut u16, 0);
        }
    }

    /// Put descriptor `id` in the available ring without publishing it.
    pub fn stage(&mut self, id: u16) {
        let slot = (self.avail_idx % self.size) as usize;
        // SAFETY: ring[slot], slot < size <= MAX, 2-byte aligned.
        unsafe { ptr::write_volatile(self.at(AVAIL + 4 + 2 * slot) as *mut u16, id) };
        self.avail_idx = self.avail_idx.wrapping_add(1);
    }

    /// Publish everything staged: the descriptors and ring entries are in
    /// memory before the index the device reads them by.
    pub fn publish(&mut self) {
        fence(Ordering::SeqCst);
        // SAFETY: the available ring's idx, 2-byte aligned.
        unsafe { ptr::write_volatile(self.at(AVAIL + 2) as *mut u16, self.avail_idx) };
        fence(Ordering::SeqCst);
    }

    /// Tell the device the queue has new buffers (spec 4.1.5.2): its index,
    /// 16 bits at its notification address.
    pub fn notify<B: Bus>(&self, b: &mut B) -> R<(), B::Error> {
        write16(b, Region::Notify, self.notify, self.index)
    }

    /// The used ring's idx as the device last wrote it.
    pub fn used_idx(&self) -> u16 {
        // SAFETY: the used ring's idx, 2-byte aligned; the device writes it.
        let v = unsafe { ptr::read_volatile(self.at(USED + 2) as *const u16) };
        // Elements are read only after the idx that covers them.
        fence(Ordering::SeqCst);
        v
    }

    /// A used element is waiting.
    pub fn has_used(&self) -> bool {
        self.used_idx() != self.last_used
    }

    /// The next used element, `(id, len)`, without taking it.
    pub fn peek_used(&self) -> Option<(u32, u32)> {
        if !self.has_used() {
            return None;
        }
        let e = self.at(USED + 4 + 8 * (self.last_used % self.size) as usize);
        // SAFETY: ring[slot], slot < size <= MAX, 4-byte aligned; covered by
        // the idx read above.
        Some(unsafe { (ptr::read_volatile(e as *const u32), ptr::read_volatile(e.add(4) as *const u32)) })
    }

    /// Take the element `peek_used` returned.
    pub fn take_used(&mut self) {
        self.last_used = self.last_used.wrapping_add(1);
    }
}
