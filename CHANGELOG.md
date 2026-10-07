# Changelog

## [Unreleased]

### 2026-10-07
- **docs:** refreshed from the code: README Shipping now says the driver is not yet pinned in stormbootx's `build-nic-drivers.sh` (ixgbe and mlx4 only; pin offered on stormbootx#75, takeover on stormbootx#108); everything else checked against the code (IDs, features, queue/DMA sizes, console line, check app, 44 host tests)
- **fix:** check app: smoltcp `auto-icmp-echo-reply` (the peer's pings went unanswered with default features off); finds its SNP by code address if OpenProtocolInformation does not show the child; prints the PciIo opens
- **test:** `scripts/test-ovmf.sh` boots a third time with QEMU's iPXE option ROM on the NIC; verified on pve `stormnictest1` (run 80115f800a, `--ping`)

### 2026-10-06
- **docs:** licensed MIT (LICENSE added); NOTICE is now an acknowledgement of where the hardware facts were learned (an original Rust rewrite, no code copied) — owner, repo made public
- **feat:** virtio-net UEFI SNP driver (#1): EFI boot-service driver with `EFI_DRIVER_BINDING_PROTOCOL` for 1af4:1041 and 1af4:1000 with virtio 1.x capabilities; capability parsing, feature negotiation (VERSION_1 and MAC required; STATUS, ACCESS_PLATFORM, ORDER_PLATFORM accepted), split virtqueues RX 0 / TX 1 (≤ 32 entries, 2 KiB buffers), DMA check, SNP on a child handle with a MAC device path, device reset at Shutdown/Stop/ExitBootServices; software receive filters, fixed station address; quiet console with the trace behind `StormnicVerbose` (from stormnic-ixgbe)
- **test:** host tests against a simulated virtio-net device (`test/`, `scripts/test-host.sh`)
- **fix:** Start holds PciIo BY_DRIVER | EXCLUSIVE: on pve the NIC's iPXE option ROM, tried by ConnectController after ours, opened it EXCLUSIVE and forced the driver off; the check app disconnects the firmware tree leaf first and judges by the BY_DRIVER opens left (pve's OVMF returns NOT_FOUND)
- **feat:** check app (`check/`) and check image (`scripts/build-image.sh`): takes the NIC from the firmware's driver, binds stormnic-virtio, runs DHCP, ping and UDP echo over its SNP; `scripts/test-ovmf.sh` boots it under QEMU/OVMF with modern and transitional NICs
- **docs:** CLAUDE.md work plan, README
- **chore:** repository created (stormbootx#75, owner-approved); README, CHANGELOG, .gitignore
