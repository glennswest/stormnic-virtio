//! The PCI functions this driver binds (pci.ids, checked on stormbootx#75):
//! a virtio network device whose transport is virtio 1.x.

/// `1af4:1041`: "Virtio 1.0 network device" (modern only, PCI device ID
/// 0x1040 + device type 1, virtio spec 4.1.2).
pub const MODERN: u16 = 0x1041;
/// `1af4:1000`: "Virtio network device", the transitional ID. Bound only
/// when it carries the virtio 1.x capabilities (QEMU's default since 2.7;
/// Supported checks); a legacy-only device is left alone.
pub const TRANSITIONAL: u16 = 0x1000;
/// PCI class: network controller.
pub const CLASS_NETWORK: u8 = 0x02;

pub struct Nic {
    pub device: u16,
    pub name: &'static str,
}

pub const SUPPORTED: &[Nic] = &[
    Nic { device: MODERN, name: "virtio-net (modern)" },
    Nic { device: TRANSITIONAL, name: "virtio-net (transitional)" },
];

pub fn lookup(vendor: u16, device: u16) -> Option<&'static Nic> {
    if vendor != crate::virtio::VENDOR {
        return None;
    }
    SUPPORTED.iter().find(|n| n.device == device)
}
