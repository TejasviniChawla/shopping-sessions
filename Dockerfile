# Shopping Sessions: single static Rust binary, FROM scratch.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# Static glibc binary tuned for the scorer's Cascade Lake Xeons (AVX-512 available).
# x86-64-v3 (AVX2+BMI2) runs on every Cascade Lake+ scorer; Rosetta cannot emulate AVX-512.
ARG TARGET_CPU=x86-64-v3
ENV RUSTFLAGS="-C target-feature=+crt-static -C target-cpu=${TARGET_CPU}"
# Passing --target explicitly makes +crt-static apply to the final link (static glibc binary).
RUN T=$(rustc -vV | sed -n 's/^host: //p') \
 && cargo build --release --target "$T" \
 && strip "target/$T/release/shopsess" \
 && cp "target/$T/release/shopsess" /shopsess

FROM scratch
COPY --from=build /shopsess /shopsess
USER 1000:1000
ENV DATA_DIR=/data OUTPUT_DIR=/output SCRATCH_DIR=/scratch
CMD ["/shopsess"]
