# stormnic-virtio

A Rust `no_std` UEFI Simple Network Protocol (SNP) driver for **virtio-net**
(modern, virtio 1.x PCI, device ID 1af4:1041), loaded by
[stormbootx](https://github.com/glennswest/stormbootx) so it can claim and boot
images over virtio NICs (pve VMs, QEMU) without iPXE. It's a sibling of
stormnic-ixgbe and stormnic-mlx4, built and pinned the same way: stormbootx's
`build-nic-drivers.sh` → the nic-drivers registry image → the rustnic media.

Work starts from stormbootx#75.
