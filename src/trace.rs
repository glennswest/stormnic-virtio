//! The last few suppressed trace lines (stormnic-ixgbe#22), so a failure can still show
//! the steps that led to it when the console is quiet. UEFI-independent;
//! `console.rs` owns the one instance.

use alloc::collections::VecDeque;
use alloc::string::String;

/// How many trace lines a failure replays: one Start's bring-up writes
/// about 12 (reset, link setup, DMA, check), so this covers the whole Start.
pub const KEEP: usize = 16;

pub struct Ring {
    lines: VecDeque<String>,
    /// Lines pushed out of the ring since the last `clear`, so the replay
    /// can say that earlier steps are missing.
    pub dropped: usize,
}

impl Ring {
    pub const fn new() -> Self { Ring { lines: VecDeque::new(), dropped: 0 } }

    pub fn push(&mut self, line: String) {
        if self.lines.len() == KEEP {
            self.lines.pop_front();
            self.dropped += 1;
        }
        self.lines.push_back(line);
    }

    /// Forget everything: the start of a new Start.
    pub fn clear(&mut self) {
        self.lines.clear();
        self.dropped = 0;
    }

    /// Take the lines, oldest first, and how many were dropped before them;
    /// the ring is empty afterwards, so a later failure replays only what
    /// came after this one.
    pub fn take(&mut self) -> (VecDeque<String>, usize) {
        let dropped = self.dropped;
        self.dropped = 0;
        (core::mem::take(&mut self.lines), dropped)
    }
}
