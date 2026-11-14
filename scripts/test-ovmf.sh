#!/usr/bin/env bash
# The driver under OVMF in QEMU, on the build box (needs no root):
#
#   sc-build scripts/test-ovmf.sh
#
# Builds the check image (scripts/build-image.sh) with a UDP echo port for
# slirp, then boots it once per virtio-net flavour:
#
#   modern        virtio-net-pci,disable-legacy=on   -> 1af4:1041
#   transitional  virtio-net-pci,disable-legacy=off  -> 1af4:1000 with the
#                                                       virtio 1.x capabilities
#   modern-ipxe   1041 with QEMU's iPXE option ROM (efi-virtio.rom), as pve has
#
# Fedora's OVMF carries VirtioNetDxe: the check app connects it to the NIC
# first (as stormbootx's first ConnectController pass does), then has to take
# the NIC from it and bind ours (check/src/main.rs).
# slirp leases 10.0.2.15 and answers pings to its 10.0.2.2; the UDP echo
# goes to a python echo server on the build box's loopback, which slirp
# reaches as 10.0.2.2. Each boot must print `STORMNIC-VIRTIO CHECK PASS`
# and the driver's one console line.
#
# Needs qemu-system-x86_64, OVMF, python3, mkfs.fat, mtools. KVM when
# /dev/kvm is writable, TCG otherwise.
set -euo pipefail
OVMF_CODE=${OVMF_CODE:-/usr/share/edk2/ovmf/OVMF_CODE.fd}
OVMF_VARS=${OVMF_VARS:-/usr/share/edk2/ovmf/OVMF_VARS.fd}
LIMIT=${LIMIT:-180}
say() { printf 'test-ovmf: %s\n' "$*"; }
die() { say "FAIL: $*"; exit 1; }
command -v qemu-system-x86_64 >/dev/null || die "qemu-system-x86_64 is not installed"
[[ -r "$OVMF_CODE" ]] || die "no OVMF at $OVMF_CODE"

W=$(mktemp -d "${TMPDIR:-/tmp}/test-ovmf.XXXXXX")
ECHO_PID=""
cleanup() { [[ -n "$ECHO_PID" ]] && kill "$ECHO_PID" 2>/dev/null; rm -rf "$W"; }
trap cleanup EXIT

# A UDP echo server; port 0 lets the kernel pick, written down for the image.
python3 - "$W/echo.port" <<'PY' &
import socket, sys
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.bind(("127.0.0.1", 0))
open(sys.argv[1], "w").write(str(s.getsockname()[1]))
while True:
    data, peer = s.recvfrom(65536)
    s.sendto(data, peer)
PY
ECHO_PID=$!
for _ in $(seq 50); do [[ -s "$W/echo.port" ]] && break; sleep 0.1; done
PORT=$(cat "$W/echo.port") || die "the UDP echo server did not start"
say "UDP echo on 127.0.0.1:$PORT (10.0.2.2:$PORT from the guest)"

CHECK_CONF="udp_port=$PORT" scripts/build-image.sh "$W/check.img"

accel=tcg
[[ -w /dev/kvm ]] && accel=kvm
boot() {
    # $5: extra virtio-net-pci properties; $6: the speed part of the port
    # line they must give.
    local name=$1 legacy=$2 rom=${4-} props=${5-} speed=${6-unknown} log="$W/$1.log"
    cp "$OVMF_VARS" "$W/vars.fd"
    say "boot $name (disable-legacy=$legacy, $accel)"
    timeout "$LIMIT" qemu-system-x86_64 -machine q35,accel="$accel" -m 512 -no-reboot \
        -drive if=pflash,format=raw,readonly=on,file="$OVMF_CODE" \
        -drive if=pflash,format=raw,file="$W/vars.fd" \
        -drive file="$W/check.img",format=raw,if=none,id=disk -device virtio-blk-pci,drive=disk,bootindex=1 \
        -netdev user,id=n0 -device virtio-net-pci,netdev=n0,disable-legacy="$legacy",disable-modern=off,romfile="$rom"$props \
        -serial file:"$log" -monitor none -display none </dev/null >/dev/null 2>&1 || true
    sed -e 's/\x1b\[[0-9;]*[A-Za-z]//g' "$log" | tr -d '\r' | grep -E '^(stormnic-virtio|check:|STORMNIC-VIRTIO)' || true
    grep -q 'STORMNIC-VIRTIO CHECK PASS' "$log" || die "$name: no PASS line"
    # OVMF's VirtioNetDxe held it and had to be disconnected.
    grep -q 'firmware driver(s) disconnected' "$log" \
        || die "$name: the firmware's driver did not hold the NIC, so the takeover was not tested"
    # The stormnic contract's port line (#5): media, speed, the standard
    # FEC (none: virtual) and link UP.
    grep -Eq "stormnic-virtio [0-9.]+: [0-9a-f:.]+ 1af4:$3 .* port 0: MAC ([0-9a-f]{2}:){5}[0-9a-f]{2}, media virtual, speed $speed, FEC none, link UP" \
        <(sed -e 's/\x1b\[[0-9;]*[A-Za-z]//g' "$log" | tr -d '\r') || die "$name: no port line for 1af4:$3 with speed $speed, FEC none, link UP"
    say "$name: PASS"
}
boot modern on 1041
# The device's own speed and duplex (VIRTIO_NET_F_SPEED_DUPLEX) on the line.
boot modern-speed on 1041 "" ",speed=10000,duplex=full" "10000 Mb/s full duplex"
boot transitional off 1000
# QEMU's iPXE EFI option ROM on the NIC, as on pve: iPXE opens PciIo
# EXCLUSIVE when ConnectController tries it after ours, which forced the
# driver off before Start held PciIo EXCLUSIVE itself.
ROM=$(ls /usr/share/qemu/efi-virtio.rom /usr/share/ipxe/qemu/efi-virtio.rom 2>/dev/null | head -1 || true)
if [[ -n "$ROM" ]]; then
    boot modern-ipxe on 1041 "$ROM"
else
    say "no efi-virtio.rom on this box: the iPXE boot is skipped"
fi
say "all boots passed"
