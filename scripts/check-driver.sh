#!/usr/bin/env bash
# Build the driver and check the image is what the firmware needs: a PE32+
# EFI boot-service driver (subsystem 11), not an application (10), which the
# firmware would unload as soon as its entry point returned.
#
#   sc-build scripts/check-driver.sh
set -euo pipefail
# --locked: Cargo.lock is committed; a build must not re-resolve deps.
cargo build --locked --release --target x86_64-unknown-uefi
efi="${CARGO_TARGET_DIR:-target}/x86_64-unknown-uefi/release/stormnic-virtio.efi"
python3 - "$efi" <<'PY'
import hashlib, struct, sys
path = sys.argv[1]
data = open(path, "rb").read()
pe = struct.unpack_from("<I", data, 0x3c)[0]
assert data[pe:pe + 4] == b"PE\0\0", "not a PE image"
machine = struct.unpack_from("<H", data, pe + 4)[0]
opt = pe + 24
magic = struct.unpack_from("<H", data, opt)[0]
subsystem = struct.unpack_from("<H", data, opt + 68)[0]
names = {10: "EFI_APPLICATION", 11: "EFI_BOOT_SERVICE_DRIVER", 12: "EFI_RUNTIME_DRIVER"}
print(f"{path}: {len(data)} bytes, sha256 {hashlib.sha256(data).hexdigest()}")
print(f"  machine 0x{machine:x}, magic 0x{magic:x} (PE32+ is 0x20b), subsystem {subsystem} ({names.get(subsystem, '?')})")
assert machine == 0x8664 and magic == 0x20b, "not an x86_64 PE32+ image"
assert subsystem == 11, "not linked as an EFI boot-service driver"
print("  OK: x86_64 EFI boot-service driver")
PY
