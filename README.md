# Shopping Sessions: Rust pipeline

Single static binary, no runtime. Reads `$DATA_DIR`, writes Parquet to `$OUTPUT_DIR/user_daily` and `$OUTPUT_DIR/campaign_daily`.

Build and run locally:

    cargo build --release
    DATA_DIR=../dev OUTPUT_DIR=out NCPU=8 target/release/shopsess

Design notes: `docs/NOTES.md`. Prior-art research: `docs/research.md`.
