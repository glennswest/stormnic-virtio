#!/usr/bin/env bash
# The UEFI-independent code against the simulated device, on the build box:
#
#   sc-build scripts/test-host.sh
set -euo pipefail
mkdir -p target
for t in virtio net snp trace; do
    rustc --edition=2021 --test "test/$t.rs" -o "target/$t-tests"
    "target/$t-tests"
done
