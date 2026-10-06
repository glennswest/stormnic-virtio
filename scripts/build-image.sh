#!/usr/bin/env bash
# Build the driver and the check app, and lay them on a bootable disk image:
# a GPT disk (512-byte sectors) with one FAT16 EFI system partition holding
#
#   \EFI\BOOT\BOOTX64.EFI                    the check app (check/)
#   \stormboot\drivers\stormnic-virtio.efi   the driver, where stormbootx keeps it
#   \stormnic-check.conf                     only when CHECK_CONF is set (its text)
#
#   scripts/build-image.sh [OUT]   (default target/check.img)
#   SC_BUILD_OUT=target/check.img SC_BUILD_OUT_TO=tmp/check.img sc-build scripts/build-image.sh
#
# Needs cargo with the x86_64-unknown-uefi target, python3, mkfs.fat and mtools.
set -euo pipefail
OUT=${1:-target/check.img}
T=${CARGO_TARGET_DIR:-target}
export MTOOLS_SKIP_CHECK=1

cargo build --locked --release --target x86_64-unknown-uefi
cargo build --locked --release --target x86_64-unknown-uefi -p stormnic-virtio-check
DRIVER="$T/x86_64-unknown-uefi/release/stormnic-virtio.efi"
CHECK="$T/x86_64-unknown-uefi/release/stormnic-virtio-check.efi"

W=$(mktemp -d "${TMPDIR:-/tmp}/check-image.XXXXXX")
trap 'rm -rf "$W"' EXIT
# 16 MiB FAT16 ESP.
mkfs.fat -C -F 16 -n STORMNIC "$W/esp.img" 16384 >/dev/null
mmd -i "$W/esp.img" ::/EFI ::/EFI/BOOT ::/stormboot ::/stormboot/drivers
mcopy -i "$W/esp.img" "$CHECK" ::/EFI/BOOT/BOOTX64.EFI
mcopy -i "$W/esp.img" "$DRIVER" ::/stormboot/drivers/stormnic-virtio.efi
if [[ -n "${CHECK_CONF:-}" ]]; then
    printf '%s\n' "$CHECK_CONF" > "$W/stormnic-check.conf"
    mcopy -i "$W/esp.img" "$W/stormnic-check.conf" ::/stormnic-check.conf
fi
mkdir -p "$(dirname "$OUT")"
python3 - "$W/esp.img" "$OUT" <<'PY'
import struct, sys, uuid, zlib
esp, out = sys.argv[1], sys.argv[2]
bs = 512
data = open(esp, 'rb').read()
first = 2048
last = first + len(data) // bs - 1
total = last + 1 + 33
entries = bytearray(128 * 128)
entries[0:128] = (uuid.UUID('C12A7328-F81F-11D2-BA4B-00A0C93EC93B').bytes_le
                  + uuid.uuid4().bytes_le + struct.pack('<QQQ', first, last, 0)
                  + 'EFI system'.encode('utf-16-le').ljust(72, b'\0'))
ecrc = zlib.crc32(entries)
disk_guid = uuid.uuid4().bytes_le
def header(me, alt, table):
    h = struct.pack('<8sIIIIQQQQ16sQIII', b'EFI PART', 0x10000, 92, 0, 0,
                    me, alt, 34, total - 34, disk_guid, table, 128, 128, ecrc)
    return h[:16] + struct.pack('<I', zlib.crc32(h)) + h[20:]
img = bytearray(total * bs)
img[446:462] = struct.pack('<BBBBBBBBII', 0, 0, 2, 0, 0xEE, 0xFF, 0xFF, 0xFF, 1, min(total - 1, 0xFFFFFFFF))
img[510:512] = b'\x55\xaa'
img[bs:bs + 92] = header(1, total - 1, 2)
img[2 * bs:2 * bs + len(entries)] = entries
img[first * bs:first * bs + len(data)] = data
img[(total - 33) * bs:(total - 33) * bs + len(entries)] = entries
img[(total - 1) * bs:(total - 1) * bs + 92] = header(total - 1, 1, total - 33)
open(out, 'wb').write(img)
PY
echo "image: $OUT ($(stat -c %s "$OUT") bytes)"
echo "  driver $(sha256sum "$DRIVER" | cut -d' ' -f1)  stormnic-virtio.efi"
echo "  check  $(sha256sum "$CHECK" | cut -d' ' -f1)  BOOTX64.EFI"
