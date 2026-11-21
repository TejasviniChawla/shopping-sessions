# Shopping Sessions: single static Rust binary, FROM scratch, built with profile-guided optimization.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY tools/generator ./tools/generator
# x86-64-v3 (AVX2+BMI2) runs on every Cascade Lake+ scorer; Rosetta cannot emulate AVX-512.
ARG TARGET_CPU=x86-64-v3
ENV BASEFLAGS="-C target-feature=+crt-static -C target-cpu=${TARGET_CPU}"
# PGO: instrumented build -> run on a generated 1M-event set -> optimized build. Any failure falls back to a plain build.
RUN T=$(rustc -vV | sed -n 's/^host: //p') && echo "$T" > /tmp/triple
RUN set -e; T=$(cat /tmp/triple); \
    ( rustup component add llvm-tools-preview >/dev/null 2>&1 \
      && (cd tools/generator && cargo build --release -q) \
      && tools/generator/target/release/generate --seed 3 --events 1000000 --clicks 250000 --output /tmp/pgo-data \
      && RUSTFLAGS="$BASEFLAGS -C profile-generate=/tmp/pgo-prof" cargo build --release -q --target "$T" \
      && mkdir -p /tmp/pgo-out \
      && DATA_DIR=/tmp/pgo-data OUTPUT_DIR=/tmp/pgo-out NCPU=4 NO_FORK=1 "target/$T/release/shopsess" \
      && PROFDATA=$(find ~/.rustup/toolchains -name llvm-profdata | head -1) \
      && "$PROFDATA" merge -o /tmp/pgo.profdata /tmp/pgo-prof \
      && RUSTFLAGS="$BASEFLAGS -C profile-use=/tmp/pgo.profdata -C llvm-args=-pgo-warn-missing-function" cargo build --release -q --target "$T" \
      && echo "PGO build ok" ) \
    || ( echo "PGO failed; plain build" && RUSTFLAGS="$BASEFLAGS" cargo build --release -q --target "$T" ); \
    strip "target/$T/release/shopsess" && cp "target/$T/release/shopsess" /shopsess

FROM scratch
COPY --from=build /shopsess /shopsess
USER 1000:1000
ENV DATA_DIR=/data OUTPUT_DIR=/output SCRATCH_DIR=/scratch
CMD ["/shopsess"]
