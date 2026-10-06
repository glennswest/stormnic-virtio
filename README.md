# stormnic-virtio

A Rust `no_std` UEFI driver that gives firmware an
`EFI_SIMPLE_NETWORK_PROTOCOL` (SNP) for **virtio-net** over the modern
(virtio 1.x) PCI transport, so stormbootx can run its own TCP/IP (smoltcp)
on pve VMs and QEMU without iPXE or the firmware's driver. A sibling of
stormnic-ixgbe and stormnic-mlx4, built and pinned the same way: stormbootx's
`scripts/build-nic-drivers.sh` → the nic-drivers golden and the rustnic
media (stormbootx#75).

Written from the OASIS virtio specification (1.x: PCI transport 4.1, split
virtqueues 2.7, network device 5.1) and the UEFI specification. MIT.

## What it binds

| PCI ID | pci.ids | bound when |
|---|---|---|
| `1af4:1041` | Virtio 1.0 network device | always (modern only) |
| `1af4:1000` | Virtio network device (transitional) | it carries the virtio 1.x capabilities (QEMU's default since 2.7); a legacy-only device is left alone |

Supported declines a function another driver already holds `BY_DRIVER`, so
the platform's own driver wins. OVMF has one (VirtioNetDxe, over
Virtio10Dxe): on OVMF this driver runs only when the loader takes the
function from it — `DisconnectController`, then `ConnectController` with
this driver named. The check app (below) does exactly that.

## How it works

- **Start**: PciIo `BY_DRIVER`; memory decode and bus mastering
  (`src/decode.rs`, from stormnic-ixgbe); the virtio capabilities from the
  PCI capability list (common, notify, device configuration; any BAR, any
  offset); feature negotiation (spec 3.1.1): `VIRTIO_F_VERSION_1` and
  `VIRTIO_NET_F_MAC` required, `VIRTIO_NET_F_STATUS`,
  `VIRTIO_F_ACCESS_PLATFORM` and `VIRTIO_F_ORDER_PLATFORM` accepted when
  offered, nothing else (no offloads, no mergeable buffers, no control
  queue); MAC and link from the device configuration. One DMA region
  (PciIo `AllocateBuffer` + `Map` common buffer, 33 pages) holds both
  queues and their buffers. A DMA check sends one broadcast frame
  (EtherType 0x88B5, `stormnic-virtio DMA check`) and waits for its
  descriptor to come back; then the device is reset and the SNP goes on a
  child handle with the controller's device path plus a MAC node.
- **Queues**: receive queue 0 and transmit queue 1, split virtqueues of at
  most 32 entries (fewer if the device offers fewer), one 2 KiB buffer per
  descriptor, the 12-byte `virtio_net_hdr` in front of every frame (all
  zeros on transmit). Polled: no interrupts, `VIRTQ_AVAIL_F_NO_INTERRUPT`
  set, no MSI-X vector. Frames are copied in and out of the driver's own
  buffers, so no caller memory is mapped for DMA.
- **SNP** (`src/snp_core.rs`, from stormnic-ixgbe): Initialize is a full
  device initialization, Shutdown and ExitBootServices are a device reset
  (after which the device touches no queue memory). Receive filters are
  applied in software (without the control queue the device's own filter
  cannot be programmed; QEMU passes everything). The station address is
  the device's: `MacAddressChangeable` is FALSE and StationAddress accepts
  only the permanent address (anything else: `EFI_UNSUPPORTED`).
  Statistics and NvData: `EFI_UNSUPPORTED`.

## Console

One line per NIC by default:

```
stormnic-virtio 0.1.0: 0000:00:03.0 1af4:1041 virtio-net (modern): MAC 52:54:00:12:34:56, link up, queues 32/32, SNP installed
```

Always printed as well: the DMA check's descriptor not coming back, any
failure (with the trace lines before it), DMA that could not be stopped.
Everything else — Supported, capability windows, negotiated features, DMA
region, DMA check, SNP initialize/shutdown/filters, Stop — is trace,
printed when verbose: built with `--features verbose`, or the EFI variable
`StormnicVerbose` (vendor `ce1479a2-eab9-4176-b0ad-c909ea5b8e0b`, shared by
every stormnic driver) with a non-zero first byte.

## Build and test

Never on the session VM: `sc-build` after pushing.

```bash
sc-build scripts/check-driver.sh   # the .efi is a PE32+ EFI boot-service driver (subsystem 11)
sc-build scripts/test-host.sh      # the virtio core against a simulated device (test/)
sc-build scripts/test-ovmf.sh      # QEMU + Fedora's OVMF: modern and transitional NICs
SC_BUILD_OUT=target/check.img SC_BUILD_OUT_TO=tmp/check.img sc-build scripts/build-image.sh
stormcentral testhost boot stormnictest1 --image tmp/check.img \
    --expect 'STORMNIC-VIRTIO CHECK PASS' --fail 'STORMNIC-VIRTIO CHECK FAIL' --ping --timeout 300
```

**The check app** (`check/`, `stormnic-virtio-check.efi`) is the
`BOOTX64.EFI` of the check image (`scripts/build-image.sh`, a GPT disk
with one FAT ESP; the driver sits at `\stormboot\drivers\stormnic-virtio.efi`
as on stormbootx media). It finds the virtio-net function, disconnects the
firmware's driver, loads and starts ours, connects the function with it,
opens its SNP `EXCLUSIVE` and runs smoltcp: DHCP, five pings to the router,
a 1200-byte UDP echo to the router's port 7 (`udp_port=` in
`\stormnic-check.conf` changes it, `0` skips it). It prints
`STORMNIC-VIRTIO CHECK PASS: …` or `… FAIL: …`, answers pings for 15 s
more and powers off.

**pve**: `stormnictest1` is a `boot` machine (stormcentral #307) for this
project with the #310 private network: a fresh q35/OVMF VM per run, its
NIC a modern virtio-net (1af4:1041) whose other end is stormcentral at
`10.77.0.1` — DHCP (`10.77.0.10`), ping both ways, UDP/TCP echo on port 7.

## Shipping

In stormbootx's `\stormboot\drivers` on the rustnic media, from a pinned
commit in `scripts/build-nic-drivers.sh`. Not a stormcentral component (no
golden of its own).

## Licence

MIT (see LICENSE). NOTICE acknowledges the sources the hardware facts were learned from; no code was copied.
