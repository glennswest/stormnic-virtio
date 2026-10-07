#!/usr/bin/env bash
# Build stormbootx's rustnic disk medium at a stormbootx ref, with
# stormbootx's own recipe (deploy/build-golden.sh stormbootx-rustnic-disk):
# stormbootx plus the drivers its build-nic-drivers.sh pins (stormnic-virtio
# among them, stormbootx#75) and `prefer_media_drivers = virtio`
# (stormbootx#108). It is the same medium as the golden of that ref, built
# here so `testhost boot` can run it under pve's OVMF (#1).
#
#   scripts/build-stormbootx-image.sh [OUT]   (default target/stormbootx-rustnic.img)
#   SC_BUILD_OUT=target/stormbootx-rustnic.img SC_BUILD_OUT_TO=tmp/stormbootx-rustnic.img \
#     sc-build scripts/build-stormbootx-image.sh
#
# STORMBOOTX_REF (default v0.23.0, a9f1be0: golden-stormbootx-rustnic-4c485750874d219a).
# Needs git, cargo with the x86_64-unknown-uefi target, and what stormbootx's
# build-boot-agent.sh needs (mkfs.fat, mtools).
set -euo pipefail
OUT=${1:-target/stormbootx-rustnic.img}
REF=${STORMBOOTX_REF:-v0.23.0}
REPO=https://github.com/glennswest/stormbootx.git

W=$(mktemp -d "${TMPDIR:-/tmp}/stormbootx-image.XXXXXX")
trap 'rm -rf "$W"' EXIT
git -C "$W" init -q stormbootx
git -C "$W/stormbootx" fetch -q --depth 1 "$REPO" "refs/tags/$REF:refs/tags/$REF" 2>/dev/null \
    || git -C "$W/stormbootx" fetch -q --depth 1 "$REPO" "$REF"
git -C "$W/stormbootx" checkout -q "${REF}" 2>/dev/null || git -C "$W/stormbootx" checkout -q FETCH_HEAD
echo "stormbootx $REF = $(git -C "$W/stormbootx" rev-parse --short HEAD)"

"$W/stormbootx/deploy/build-golden.sh" stormbootx-rustnic-disk "$W/golden"
cat "$W/golden/BUILD"
mkdir -p "$(dirname "$OUT")"
cp "$W/golden/boot/stormbootx-rustnic-disk.img" "$OUT"
ls -l "$OUT"
