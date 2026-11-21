# Shopping Sessions solution: design and status log

## Goal
Single static Rust binary, FROM scratch image. Target < 0.6 s in-container on 20M events (8 vCPU Cascade Lake, 24 GB).
Correctness first: 36 fixtures + exact match on dev-answer (1M events, seed 1).

## Pipeline design
1. Read manifest, products.csv, campaigns.csv (small; simple parser).
2. Links: parse all link files (strict CSV dialect), intern IDs -> u32, union-find (Rem + path halving).
   Person name = byte-wise smallest ID in component (track min during a final pass).
3. Events: 8 threads pull ~2 MB byte ranges from an atomic counter across all files; snap to '\n'.
   Per line: split fields (memchr / SIMD), strict dialect check (quotes only around whole field, exactly 7 fields,
   no stray quotes), parse event_id (keep bytes), user_id, ts (F1/F2/F3), ingest_ts (F1), type, product, price.
   Emit compact record even if invalid (validity flag) because dedup happens BEFORE validation.
   Dedup by event_id keeping min ingest_ts: per-thread hash tables partitioned by hash of event_id, merged.
4. Clicks: same approach; dedup by click_id (any copy); validate; campaign lookup.
5. Map each surviving event/click user_id -> person (union-find root), counting-sort by person, sort by ts.
6. Per person merge walk: sessions (gap > 30 min between distinct ts), credit (latest click ts <= start within 7 d,
   tie smallest click_id bytes), assists (distinct campaigns with clicks in [start-7d, start]).
7. Aggregate user_daily / campaign_daily into hash maps / dense arrays, write Parquet (parquet crate, snappy or none).

## Dialect gotchas (from rules.md + fixtures)
- Only '\n' ends lines; '\r' is data. Quotes are syntax only when wrapping a whole field; "" => empty (null).
- A quote anywhere else => malformed line. Wrong field count => malformed. Empty line => malformed.
- Header row: only the first line of each file; a data row equal to the header is DATA (L3).
- Spaces are significant. IDs are case-sensitive.
- Digits are ASCII only. F3 may have leading zeros, any length; too long for i64 => just out of window (bad).
- ts window: events [ws, we), clicks [ws-7d, we). Late: ingest - ts > 24h dropped.
- price for purchase: ASCII digits only, 1..=10_000_000, leading zeros OK. Ignored for view/cart.
- Dedup before validation; kept copy = min ingest_ts (ingest_ts always valid F1).

## Status log
- 2026-11-20: read spec/starters/generator; research done (docs/research.md); dev set generated at ../dev (92 MB);
  venv at ../.venv with polars for the checker scripts. Scaffolded crate.
- 2026-11-20 20:20: first full Rust pipeline passes 36/36 fixtures and MATCHES dev-answer exactly.
  Local (M-series, 8 threads) 1M events: ~0.25-0.30 s wall. Bug fixed: partition-local uid needed base[q] offset.
- 2026-11-20 20:45: amd64 image built. target-cpu=cascadelake binary HANGS (99% CPU, no output) under Rosetta in Docker Desktop:
  Rosetta lacks AVX-512. Default now x86-64-v3 (build arg TARGET_CPU). v2 image passes fixture A1 in container.
  Local timing on 10M (928 MB) is unreliable while Docker Desktop holds 4 GB of the Mac's 8 GB (heavy swapping, sys time 5-10 s).
  Observed: dedup phase does random access into mmapped files (id/user byte compare per record) -> plan: inline 16-byte keys.
- 2026-11-20 21:15 profile (10M events, 782 MB event CSV, single thread, warm cache):
  lines-only memchr 0.19 s | +split_fields 0.95 s | +parse_event_line 1.14 s | +push to 16 partitions 1.41 s.
  Cold cache first touch of mmap: 9.1 s (85 MB/s) on this Mac -> scorer must have warm cache / fast disk for a 1 s leader.
  Whole run single-thread 9.2 s: dedup 0.9, intern 0.37, group 0.27, walk 0.25, output 0.12.
  Plan: pread into per-thread padded buffers (no mmap faults, safe SIMD over-read), SIMD u128 comma/quote masks per line,
  SWAR F1 timestamp parse, shrink records.
