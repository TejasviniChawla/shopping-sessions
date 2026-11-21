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
- 2026-11-20 21:50: pread buffers + SIMD u128 splitter. Dev 1M: 0.13 s (was 0.35). 36/36, MATCH.
  Bench warm (10.8M lines, 782 MB, 1 thread): read+lines 0.21 | +split 0.57 | +parse 0.78 | +push 1.20 s.
  Next: 48-byte EvRaw (ts u32 offset, ingest i32 s, hash recomputed from Key), fuse newline search into SIMD pass.
- 2026-11-20 22:15: 48-byte EvRaw / 40-byte ClRaw, key_hash from inline u128 (links MUST use the same hash: bug found+fixed).
  10M on Mac, 8 threads: parse 0.57 | clicks+links 0.10 | dedup 0.50 | intern 0.14 | group 0.14 | walk 0.06 | out 0.09 => 1.68 s.
  Dedup is latency-bound (random probes into 32 MB slot tables): add prefetch + winner bitmap. Parse: fuse newline into SIMD pass,
  huge-page backed partition vectors (first-touch faults), SWAR F1.
- 2026-11-20 22:45: fused newline SIMD scanner, HugeVec (mmap+MADV_HUGEPAGE) buffers, dedup prefetch + winner bitmap.
  10M Mac: parse 0.38-0.6 | dedup 0.35 | intern 0.13 | group 0.12 | walk 0.05 | out 0.11 => 1.3-1.5 s. RSS 1.05 GB.
  Parse scales only ~2.4x on 8 Mac threads (page-fault serialization suspected). Need Linux measurements (Docker VM arm64).
- 2026-11-20 23:20: Python reference (../tools/reference.py) passes 36/36 fixtures; used as an independent oracle on other seeds.
  Added fork-and-exit wrapper (Linux only; NO_FORK=1 disables). Parallel user_daily parts: output phase now ~0.07 s on 10M.
- 2026-11-20 23:30: reference.py MATCHES dev-answer (seed 1). Binary MATCHES reference on seed 7 (1M). Multi-seed check running.
- 2026-11-20 23:40: binary MATCHES reference on seeds 2,3,11,23,42,99,123,2028 (200k events each; windows 2025-2028 incl. 2028-01-23 span).
- 2026-11-21 00:10: Linux VM (Docker Desktop arm64, 4 GB) with bind-mounted data: dev MATCH with fork wrapper (exit 0), but
  timings dominated by virtiofs reads (782 MB in 4-10 s). Re-testing with data copied into a docker volume.
  SWAR 13-digit F3 parse added (tests pass, fixtures 36/36, dev MATCH).
- 2026-11-21 00:30: Linux VM with data in a docker volume (THP=always): 10M in 2.5-3.2 s in-container (parse 0.8, dedup 0.7-1.4!),
  docker run overhead ~0.35 s. VM shares cores with host, 4 GB total: directional only.
  Plan: PKey = u64 packed ID (base-37 over [0-9a-z], <=12 chars; else fallback index into per-thread side table of Key).
  Record sizes: EvRaw 48->32, ClRaw 40->24, EvValid 32->24, EvMid 24->16, EvFinal 24->12, ClFinal 24->16.
- 2026-11-21 01:10: PKey (u64 packed IDs) landed. Bug found by fixtures E19/L3: fallback keys must hash by BYTES (hash/hash_in).
  10M Mac: 0.88 s total (parse 0.27-0.35, dedup 0.2, intern 0.06, group 0.1, walk+out 0.08). RSS 0.8 GB.
- 2026-11-21 01:40: SWAR pack8 for 8/9-char IDs. 8 seeds MATCH reference. Mac timings vary 0.9-2.0 s on 10M depending on
  Docker Desktop / thermal state: this machine cannot benchmark reliably. Need an x86 Linux box (GitHub Actions or a VM).
- 2026-11-21 03:00: public repo created (user approved): https://github.com/TejasviniChawla/shopping-sessions. First x86 CI bench launched.
- 2026-11-21 03:50: first x86 CI (Ice Lake 8370C, 4 vCPU, 10M): 1.58 s in-container: parse 0.64 | clicks+links 0.13 | dedup 0.24 |
  intern 0.15 | persons 0.09 | group 0.12 | walk+out 0.19. Then: branchless quote handling, lex_key (no base-37 decode in naming),
  parallel link resolution, P=64 partitions, per-thread grouping histograms. Mac 10M best 0.89 s.
- 2026-11-21 04:30: CI (EPYC 7763, 4 vCPU, P=16 branchless): 1.34 s in-container (parse 0.60, dedup 0.18, intern 0.11, group 0.11,
  walk+out 0.18); docker overhead there ~0.17 s. P=64 run OOM-killed in dedup: P*P=4096 tiny HugeVecs each rounded to a 2 MB
  THP page = 8 GB+. Fix: huge pages only for >= 8 MB allocations; P=32.
- 2026-11-21 05:00: CI Ice Lake single-thread: lines 0.21 | +split 0.26 | +parse 0.41 | +push 0.41 s per 10.8M lines.
  Added Scatter (16-record L1 staging, 512 B bursts) for parse and dedup outputs. Mac 10M best 0.81 s.
- 2026-11-21 06:00: GNR CI single-thread per 10.8M lines: lines 0.18 | split +0.19 | timestamps +0.27 | keys +0.11 | push ~+0.08.
  Added per-thread date cache (civil19_cached) and sketch-based dedup (2-bit counters, 16 slots/record; exact table only for
  candidates). Prefetch D=16. Intern table est shrunk. All checks pass.
- 2026-11-21 07:30: SCORER RESULT for commit 7526431: 1.79 s (leader 1.0). CI Zen4-4vCPU-10M in-container ~1.08 s => scorer ~1.4 + 0.35 docker.
  Since: pass-2 prefetch, packed events (16/12/8 B), links mirrored into both partitions (removes 32x scan of all links in intern).
- 2026-11-21 08:30: commit 4438fa6 CI (Zen4 4vCPU 10M): 1.01-1.06 s in-container: parse 0.375 | clicks+links 0.105 | dedup 0.18-0.24
  (pass2 0.4-0.6 thread-s dominates) | intern 0.09 | persons 0.06 | group 0.05 | walk 0.09 | out 0.05.
  Lesson: never run commit/push in a background chain while editing; a chain's `git add -A` swallowed unrelated edits twice.
  Experiments in flight: USE_MMAP=1 (mmap+MAP_POPULATE per unit), dedup isolation toggles.
- 2026-11-21 ~10:00: CI 7763 (Zen3) for 4922f03: 1.33 s in-container (parse 0.57, dedup 0.22, intern 0.11, persons 0.06, group 0.07,
  walk 0.11, out 0.06) ~= flat vs the earlier 7763 run. 10 seeds MATCH. Populate hurt (4K pages before madvise) -> opt-in only.
  PLAN (restructure): links parsed first -> global concurrent user table (CAS) + UF -> person ids for link IDs;
  dedup pass 2 does table.get_or_insert(user) -> person id directly -> EvMid{ts,person,packed} per thread + per-thread
  person histograms; drop EvValid scatter, intern phase and separate histogram pass. Expected -0.1..0.15 s on Zen4-4vCPU-10M.
- 2026-11-21 ~11:00: d200c39 (global lock-free user table, 16B slots; fused dedup/intern/histogram) on Ice Lake 8370C CI:
  1.37 s in-container (links 0.15 | parse 0.50 | clicks 0.12 | dedup+intern 0.35 | group 0.07 | walk 0.12 | out 0.06).
  Earlier 8370C runs: 1.58 (first), 1.46-1.51 (P=32 build). Submitted d200c39 as the second practice run.
