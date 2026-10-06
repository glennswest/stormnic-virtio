//! stormnic-virtio: an `EFI_SIMPLE_NETWORK_PROTOCOL` driver for virtio-net
//! (virtio 1.x over PCI), in Rust.
//!
//! An EFI boot-service driver (see build.rs). The entry point installs an
//! `EFI_DRIVER_BINDING_PROTOCOL` on the image handle (binding.rs) and returns;
//! the firmware's `ConnectController` then calls Supported/Start for each
//! controller. Start checks the device, negotiates virtio 1.x, runs a DMA
//! check and puts an `EFI_SIMPLE_NETWORK_PROTOCOL` on a child handle; the
//! user (stormbootx's smoltcp, or the check app) opens it EXCLUSIVE.
#![no_main]
#![no_std]

extern crate alloc;

// First: its macros (say!, trace!, fail!, note!) are used by the modules below.
#[macro_use]
mod console;
mod binding;
mod decode;
mod ids;
mod pci_io;
mod snp;
mod trace;
mod virtio;

use uefi::prelude::*;

#[entry]
fn main() -> Status {
    if uefi::helpers::init().is_err() {
        return Status::LOAD_ERROR;
    }
    console::init();
    let version = env!("CARGO_PKG_VERSION");
    match binding::install() {
        Ok(()) => {
            trace!("stormnic-virtio {version}: driver binding installed ({} virtio-net device IDs)", ids::SUPPORTED.len());
            Status::SUCCESS
        }
        Err(e) => {
            say!("stormnic-virtio {version}: driver binding not installed: {:?}", e.status());
            e.status()
        }
    }
}
