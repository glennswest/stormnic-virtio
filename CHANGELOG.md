# Changelog

## [Unreleased]

### 2026-11-14
- **docs:** refreshed from the code (v0.2.0 since 2026-11-07): console, features, queues, Known limits (#3, #4) rechecked against `src/`, unchanged; README Shipping says the stormbootx pin (1a04808) predates the port line and the pin to 2dcafd3 is open on stormbootx#210. Nothing the docs promise is missing from the code, so no new issue

### 2026-11-13
- **docs:** #5 verified (test-ovmf 4 boots, pve run c9a5b846b8); stormbootx pin asked on stormbootx#210

## [v0.2.0] — 2026-11-13

### Added
- The stormnic contract (#5, stormbootx#202): one line per port — driver, PCI address, PCI ID, MAC, media (`virtual`), speed, the standard FEC (`none`: a virtual port has none) and link; a port without link is an error line. `VIRTIO_NET_F_SPEED_DUPLEX` accepted when offered, its speed and duplex on the line (`unknown` otherwise). Queue sizes moved to the trace
- Host tests for speed/duplex, the standard FEC and the port line; `scripts/test-ovmf.sh` checks the port line on every boot and boots once with QEMU `speed=10000,duplex=full`

## [v0.1.0] — 2026-10-06 … 2026-11-07 (untagged)

### 2026-11-07
- **docs:** refreshed from the code (only #1's close-out landed since 2026-10-13): README Known limits names the fixed 1500 MTU (#4) and the throughput gap to VirtioNetDxe (#3); everything else rechecked, unchanged
- **docs:** #1 closed: the owner took pve run 8935d0a1dd (stormbootx v0.23.0 rustnic medium) as its last check; booting a release through stormbootx belongs to stormbootx (#106, #107)

### 2026-10-07
- **test:** verified on pve `stormnictest1` under stormbootx v0.23.0's rustnic medium (run 8935d0a1dd): taken from pve OVMF's driver, SNP installed, leased 10.77.0.10
- **docs:** README Shipping: pinned in stormbootx v0.23.0 (stormbootx#75) and taken from the firmware on the rustnic media (stormbootx#108); how to boot that medium on pve
- **test:** `scripts/build-stormbootx-image.sh`: stormbootx's rustnic disk medium at a stormbootx ref (default v0.23.0, which pins this driver), built with stormbootx's own golden recipe, for a `testhost boot` under pve's OVMF (#1)
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
