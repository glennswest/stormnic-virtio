# CLAUDE.md — stormnic-virtio

A `no_std` UEFI driver giving firmware an `EFI_SIMPLE_NETWORK_PROTOCOL` for
**virtio-net** over the modern (virtio 1.x) PCI transport: 1af4:1041, and
1af4:1000 (transitional) when it carries the virtio 1.x capabilities. Loaded
by stormbootx from `\stormboot\drivers` like its siblings stormnic-ixgbe and
stormnic-mlx4 (stormbootx#75). Read README.md first.

Read the cross-project rules in `../CLAUDE.md` first. In particular, **build
with `sc-build` after pushing, never on this VM and never as root**, and
scratch files go in `tmp/`.

## Rules for this crate

- **Sources:** the OASIS virtio specification (1.x: PCI transport section
  4.1, split virtqueues section 2.7, network device section 5.1) and the UEFI
  specification (driver binding, PCI I/O, SNP). **Never translated from
  Linux, iPXE or EDK2's VirtioNetDxe**: the crate is MIT. Reading them for
  behaviour is fine.
- It is a *driver*: the binary is an EFI boot-service driver (subsystem 11)
  installing `EFI_DRIVER_BINDING_PROTOCOL`. Supported declines a function
  another driver already holds BY_DRIVER: the platform's own driver wins.
  Under OVMF that is VirtioNetDxe; to run ours, a loader disconnects the
  function first and connects it with this driver (the check app does; for
  stormbootx see the work plan).
- Same layout as stormnic-ixgbe: the virtio core (`src/virtio.rs`,
  `src/queue.rs`, `src/net.rs`, `src/snp_core.rs`) is UEFI-independent and
  tested on the host against a simulated device (`test/`); `src/binding.rs`,
  `src/snp.rs`, `src/pci_io.rs` are the UEFI glue.
- One line per NIC on the console by default, the trace behind
  `StormnicVerbose` / `--features verbose` (stormnic-ixgbe#22).

## Build

```bash
sc-build scripts/check-driver.sh     # build + check PE32+ subsystem 11
sc-build scripts/test-host.sh        # simulated-device tests
sc-build scripts/test-ovmf.sh        # QEMU + OVMF: the check app leases and pings
SC_BUILD_OUT=target/check.img SC_BUILD_OUT_TO=tmp/check.img sc-build scripts/build-image.sh
```

## Test

- Host: `test/*.rs`, compiled with plain `rustc --test` (scripts/test-host.sh).
- QEMU/OVMF (`scripts/test-ovmf.sh`): the check image under Fedora's OVMF
  with slirp, modern-only (1041) and transitional (1000) devices. The check
  app (`check/`) disconnects OVMF's VirtioNetDxe, loads
  `\stormboot\drivers\stormnic-virtio.efi`, connects it, runs smoltcp over
  its SNP: DHCP, ping the gateway, UDP echo, then prints
  `STORMNIC-VIRTIO CHECK PASS`.
- pve with the #310 network peer: `stormcentral testhost boot <machine>
  --image tmp/check.img --expect 'STORMNIC-VIRTIO CHECK PASS' --fail
  'STORMNIC-VIRTIO CHECK FAIL' --ping`.

## Version

`Cargo.toml` → `package.version` (and `check/Cargo.toml`). Current: `v0.1.0`.

## Work plan

- [ ] #1 virtio-net SNP driver (in progress, 2026-10-06)
  - [x] Scaffold: Cargo workspace (driver + `check/`), build.rs subsystem 11
        for the driver bin only, console/trace from stormnic-ixgbe, PciIo,
        decode, scripts/check-driver.sh
  - [x] Virtio core, SNP core and glue (7e12648)
  - [x] Host tests: 44 pass (`sc-build scripts/test-host.sh`, 2026-10-06)
  - [x] QEMU/OVMF (`scripts/test-ovmf.sh`, b178b65): modern 1041 and
        transitional 1000 each take the NIC from Fedora OVMF's VirtioNetDxe,
        lease 10.0.2.15, ping 5/5, UDP echo 1200 B. check-driver: 55,808 B,
        subsystem 11, no warnings.
  - [x] pve `stormnictest1` (boot machine registered 2026-10-06 via the agent
        token: vmid 3302, #310 net, projects [stormnic-virtio]): **passed**,
        run 80115f800a at 17874cf (2026-10-07): lease 10.77.0.10, ping 5/5,
        UDP echo 1200 B, peer pings 9/9 answered. What the failed runs
        taught (3c89257aeb … 99e2be17a1): pve's OVMF puts its full network
        stack on VirtioNetDxe, and DisconnectController returns NOT_FOUND
        even when it worked (judge by the BY_DRIVER opens left); the NIC's
        iPXE option ROM opens PciIo EXCLUSIVE when ConnectController tries it
        after ours, so Start holds PciIo BY_DRIVER | EXCLUSIVE (9f90b8e);
        smoltcp needs `auto-icmp-echo-reply` with default features off.
  - [ ] stormbootx: issue to pin the driver and to let it take virtio NICs
        from OVMF's VirtioNetDxe (draft in tmp/stormbootx-issue.md), then
        pvetest1/2 boot a release through it; #1 proposed after it
