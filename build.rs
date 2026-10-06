//! Link the driver as an EFI boot-service driver, not an application.
//!
//! The PE32+ subsystem decides how the firmware loads the image (UEFI spec
//! 2.1.2): an application is unloaded when its entry point returns, a
//! boot-service driver stays resident in boot-services memory. The driver
//! binding installed by the entry point holds pointers into this image, so it
//! must stay. rustc's UEFI targets link `/SUBSYSTEM:EFI_APPLICATION` by
//! default; a later `/SUBSYSTEM` on the lld-link command line wins.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.ends_with("-uefi") {
        println!("cargo:rustc-link-arg-bin=stormnic-virtio=/SUBSYSTEM:EFI_BOOT_SERVICE_DRIVER");
    }
}
