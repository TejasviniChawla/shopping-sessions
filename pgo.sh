#!/bin/sh
# Profile-guided optimization: instrumented build, profiling run on a generated 1M-event set, optimized build.
set -e
T="$1"
rustup component add llvm-tools-preview
(cd tools/generator && cargo build --release -q)
tools/generator/target/release/generate --seed 5 --events 3000000 --clicks 750000 --output /tmp/pgo-data
RUSTFLAGS="$BASEFLAGS -C profile-generate=/tmp/pgo-prof" cargo build --release -q --target "$T"
mkdir -p /tmp/pgo-out
DATA_DIR=/tmp/pgo-data OUTPUT_DIR=/tmp/pgo-out NCPU=4 NO_FORK=1 "target/$T/release/shopsess"
PROFDATA=$(find / -name llvm-profdata -type f 2>/dev/null | head -1)
echo "profdata tool: $PROFDATA"
"$PROFDATA" merge -o /tmp/pgo.profdata /tmp/pgo-prof
ls -la /tmp/pgo.profdata
RUSTFLAGS="$BASEFLAGS -C profile-use=/tmp/pgo.profdata -C llvm-args=-pgo-warn-missing-function" cargo build --release -q --target "$T"
