//! Console output (stormnic-ixgbe#22): one line per NIC by default, the bring-up trace
//! behind a verbose switch.
//!
//! - `say!`: always printed: the per-NIC summary, warnings and errors.
//! - `trace!`: printed only when verbose. Otherwise the line is kept in a
//!   ring of the last `trace::KEEP` lines (trace.rs).
//! - `fail!`: a failure. When quiet, the kept lines are printed first, so
//!   the console still shows the steps that led to it.
//! - `note!(warn, …)`: `say!` when `warn`, else `trace!`.
//!
//! Verbose is on when the driver is built with `--features verbose`, or
//! when the EFI variable `StormnicVerbose` under `VENDOR` exists and its
//! first byte is not 0. The variable is read once, at the entry point. It is
//! one GUID for every stormnic driver, so one switch turns them all verbose.
//! stormbootx can set it (volatile, boot-service access) before it loads
//! drivers, and from the UEFI shell:
//! `setvar StormnicVerbose -guid ce1479a2-eab9-4176-b0ad-c909ea5b8e0b -bs =01`.

use crate::trace::Ring;
use alloc::format;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};
use uefi::boot::{self, Tpl};
use uefi::runtime::{self, VariableVendor};
use uefi::{cstr16, guid, Guid};

/// Vendor GUID of `StormnicVerbose`, shared by the stormnic drivers.
pub const VENDOR: Guid = guid!("ce1479a2-eab9-4176-b0ad-c909ea5b8e0b");

static VERBOSE: AtomicBool = AtomicBool::new(cfg!(feature = "verbose"));

struct Shared(UnsafeCell<Ring>);
// SAFETY: boot services run on one processor; `with_ring` keeps event
// notifications out while the ring is borrowed.
unsafe impl Sync for Shared {}
static RING: Shared = Shared(UnsafeCell::new(Ring::new()));

fn with_ring<T>(f: impl FnOnce(&mut Ring) -> T) -> T {
    // SAFETY: nothing here logs above TPL_NOTIFY (the SNP calls and the
    // ExitBootServices event run at TPL_CALLBACK), so raising to NOTIFY is
    // never a lowering; it keeps every other logger out until the guard drops.
    let _tpl = unsafe { boot::raise_tpl(Tpl::NOTIFY) };
    // SAFETY: the only borrow while the TPL is raised.
    f(unsafe { &mut *RING.0.get() })
}

/// Read the runtime switch. Called once, from the entry point.
pub fn init() {
    if verbose() { return; }
    let mut buf = [0u8; 16];
    if let Ok((data, _)) = runtime::get_variable(cstr16!("StormnicVerbose"), &VariableVendor(VENDOR), &mut buf) {
        if data.first().is_some_and(|b| *b != 0) { VERBOSE.store(true, Ordering::Relaxed); }
    }
}

pub fn verbose() -> bool { VERBOSE.load(Ordering::Relaxed) }

/// A new Start (or the end of one that succeeded): a later failure replays
/// only its own steps.
pub fn begin() {
    if !verbose() { with_ring(|r| r.clear()); }
}

pub fn trace_line(args: core::fmt::Arguments) {
    if verbose() {
        uefi::println!("{args}");
    } else {
        let line = format!("{args}");
        with_ring(|r| r.push(line));
    }
}

/// Print the kept trace lines ahead of a failure; nothing when verbose,
/// where they were printed already.
pub fn replay() {
    if verbose() { return; }
    let (lines, dropped) = with_ring(|r| r.take());
    if lines.is_empty() { return; }
    uefi::println!(
        "stormnic-virtio: the {} step(s) before the failure below{}:",
        lines.len(),
        if dropped > 0 { format!(" ({dropped} earlier not kept)") } else { alloc::string::String::new() }
    );
    for line in lines { uefi::println!("{line}"); }
}

macro_rules! say { ($($t:tt)*) => { uefi::println!($($t)*) } }
macro_rules! trace { ($($t:tt)*) => { $crate::console::trace_line(format_args!($($t)*)) } }
macro_rules! fail { ($($t:tt)*) => {{ $crate::console::replay(); uefi::println!($($t)*); }} }
#[allow(unused_macros)]
macro_rules! note {
    ($warn:expr, $($t:tt)*) => { if $warn { say!($($t)*) } else { trace!($($t)*) } }
}
