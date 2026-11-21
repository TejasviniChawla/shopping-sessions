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
- 2026-11-21 ~23:20 ET: SCORER: d200c39 (global user table restructure) = 2.17 s, WORSE than 7526431 (1.79 s). Zen3 CI agreed
  (1.46 vs 1.33). Cross-runner CI comparisons misled; only same-runner A/B from now on. Reverting src/ to the 4922f03 design.
- 2026-11-21 ~23:40 ET: same-runner A/B (GNR 6975P-C): a805d87 (per-partition design + post-7526431 improvements) 0.94 s vs
  7526431 1.02-1.06 s => ~9% faster. Recommended a805d87 for the next submission (expected ~1.6 s on the scorer).
- 2026-11-21 23:30 ET: SCORER: a805d87 = 1.69 s (new best; A/B predicted ~9%, got 6%). perf on CI: user 3.57 s / sys 1.21 s,
  161k page faults => kernel time ~25% of CPU. Fix in A/B: reuse dedup scratch across partitions, MADV_POPULATE_WRITE after
  MADV_HUGEPAGE for >=2MB HugeVecs.
- 2026-11-22 ~01:15 ET: pool build 2cf679a A/B +5% vs a805d87 (recommended; 10 seeds MATCH). f747a2d (pooled intern slots,
  u64 event_type compare) +2% vs 2cf679a on Sapphire Rapids. Next: UNIT_KB experiment (L2-resident parse buffers).
- 2026-11-22 ~02:00 ET: UNIT_KB experiment (same runner, native): 512 KB units halve sys time (1.02 -> 0.50 s) and cut total ~10%.
  Batch 574a577 (512 KB units, 2 MB-aligned maps, pinning, dedup||clicks/links overlap, fast ingest) A/B: 0.71-0.74 vs 0.73-0.82 (f747a2d).
  Replacing arrow/parquet crates with a minimal PLAIN parquet writer (src/pq.rs), validated with polars + pyarrow.
- 2026-11-22 ~03:30 ET: FOUND: wall - in-container gap depends on OUTPUT BYTES on the bind mount (docker flushes dirty pages
  at exit): uncompressed parquet (23 MB) costs +0.15 s vs Snappy (9 MB). tmpfs output: no gap. => keep output small (Snappy,
  INT32 where allowed). Do NOT submit c6d9bbd/cd1ddca/50b5fa0 (uncompressed writer). 574a577 recommended.
- 2026-11-22 ~04:30 ET: SCORER 574a577 = 1.62 s. 6925b5e (Snappy writer) A/B: -5% in-container, -3% wall, exit gap gone; 10 seeds OK.
  342c221 (dictionary user_id + INT32 counts) neutral vs 6925b5e; 10 seeds OK; recommended submission.
  CI gains translate to the scorer at ~40%: large fixed cost (container start + output flush) on the scorer.
- 2026-11-22 ~05:30 ET: x86 isolated bench (Zen3, ns/line): scan 20 | split 19 | ts 10 | ingest 5 | keys 8 | type/hash 8 | push 15.
  Block-based scanner prototype is NOT faster than the per-line 128-byte masks (22.9 vs 20.2 ns/line realistic): dropped.
  Batch: intern-table prefetch; person histogram derived from per-uid counts gathered during intern (no record pass).
- 2026-11-22 ~06:00 ET: SCORER 342c221 = 1.88 s (vs 574a577 1.62) despite same-runner CI -5%: attributed to the organizers'
  stated +-20% machine variance (queue wait 524 ms). Practice board is noisy; the final retimes. Choose by same-runner A/B.
  Dedup candidate ordering experiment: linear probing with slot-sorted inserts is quadratic (pass3 15 s) -> reverted.
  u64-halves splitter: -1 ns/line only (not integrated). Container overhead of our image on CI: 0.10 s.
- 2026-11-22 ~07:30 ET: remaining plan (ranked): PGO in Docker build (~5%); mmap input with fault-around (~2-5%);
  measure barrier idle; free raw partitions as dedup finishes (1-2%); skip mirrored links in UF (~1%); stamp cache (~2%).
  Ruled out: block scanner, AVX-512, global table, sorted probes, oversubscription, parallel unmap, bulk populate.
- 2026-11-22 ~08:30 ET: 4a83518 (u64-halves split, compact camp cells) A/B +4% vs 8ff776b; 10 seeds OK -> recommended.
  Idle report: walk phase 22% idle (whales at the tail) -> heavy-persons-first scheduling (idle 5-10%).
  PGO added to the Dockerfile (instrumented run on a generated 1M set; automatic fallback to a plain build).
- 2026-11-22 ~09:30 ET: PGO Docker build works (traced pgo.sh; "PGO build ok" ~23 s extra). A/B vs plain 4a83518 pending.
  mmap input (fault-around) neutral vs pread: not adopted. Heavy-first scheduling + mirror skip: ~1% (4325cc0 lineage).
  CI idle: parse 1%, dedup 1%, walk 5-10% after the fix.
- 2026-11-22 ~10:30 ET: PGO A/B vs plain 4a83518 on one runner: 0.806-0.813 vs 0.839-0.861 (-5%). 3M vs 1M training: identical.
  Recommended: 2bf102e (or 1627d27, equivalent). Plateau of this architecture on Zen4-4vCPU-10M ~0.80 s in-container.
  Dropped ideas: stamp-value cache (shuffled input, 1.2M distinct seconds -> no locality), 28-byte raw records (~1%).
- 2026-11-21 ~00:00 ET (clock corrected from CI timestamps): SURVEY of fast public entries (Xero 0.90, Eagle Eye 0.94, Teti 1.01): 3 materializations, person resolved
  at dedup time via flat prefetched link-id map + negative filter (no intern phase), u64 order-preserving keys, mmap input
  (scorer /data reportedly tmpfs), hand-rolled uncompressed parquet, _exit. Branch v3 = restructure to that shape.
- 2026-11-21 ~00:40 ET: v3 landed on branch v3 (047e9a1): parse -> 32 B raw by event id -> dedup resolves person via LinkMap
  (flat open-addressed key->name + bit filter, prefetched) and emits 16 B PRec by person -> person partition does local ids,
  counting sort, per-person sort+walk, per-thread parquet. Fallback (non-packable) IDs now go to a global mutex registry, so
  PKey compares by u64 only (no Side tables). 36/36 fixtures, dev MATCH. Mac stage log 10M: parse 0.46, dedup 0.17, click
  dedup 0.02, persons 0.10, total 0.76 s (v2 1.10 s same run). CI A/B vs 2bf102e: run 44877150577. Ten-seed check running.
  Old main.rs saved as docs/main_v2_final_backup.rs.txt.
- 2026-11-21 ~01:30 ET: CI A/B of first v3 (047e9a1) vs 2bf102e: LOSS 1.23-1.26 vs 1.18-1.21 wall (Zen3 4 vCPU 10M). Stage
  compare: v3 saved intern+persons (-0.10) but parse +0.06 (link-map thread oversubscribed) and dedup +0.05. v3 ten seeds MATCH.
  Second survey (top repos: hansliu-dev/676767 = team 67 0.59 s; Dremix10 C++ 0.66 s; zlyuan9 0.37-0.44 s native 20M with
  dedup 50 ms / persons 75 ms): the fast entries use 1024-2048 partitions so dedup tables sit in L2, 64 B write-combining
  lines with NT stores (Vibhor: -12-15% on board), huge-page arenas, 30-min bins instead of sort.
  4145b80: runtime partition count (LOG_P env override), Scatter = one arena per producer + 64 B staging lines + NT stores
  (Parts<T> readers), dedup = per-partition sub-buckets (~8k recs) with one L2 table, no sketch; link map built as task 0 of
  the event-parse queue. Mac 10M: LOG_P=6 0.49 s total (parse 0.255!), 8: 0.67, 10: 1.08 -> fewer write streams win on the
  Mac; CI sweep LOG_P 6/7/8(default)/9/10 + A/B vs 2bf102e: run 44878125122. Fixtures 36/36, dev MATCH at 6 and 10.
- 2026-11-21 ~02:00 ET: CI A/B 4145b80 vs 2bf102e on Zen4 9V45 (4 vCPU, 10M): WIN 0.73/0.75 vs 0.78/0.79/0.82 wall;
  in-process 0.57 vs 0.61. Stages (4 thr): parse+linkmap 0.27 (old parse 0.24: still faster), dedup+click parse 0.15,
  click dedup 0.025, persons+parquet 0.12. LOG_P sweep native: 6: 0.60, 7: 0.59, 8/9/10: 0.565-0.58 -> keep 8.
  Next: symbolized perf profile (workflow now builds unstripped native binary), then parse hot spots.
- 2026-11-21 ~03:30 ET: main fast-forwarded to v3 (c281c15). Source-line profile: no line > 4%; link-map bit filter stall
  ~4.5%, pack8 table lookups ~3.6%, hash ~2.3%, ts SWAR ~3.3%, split ~3%, sorts ~3%, snappy+pq ~3%, kernel ~8%.
  6a56d39 (no filter, 1-multiply hash, SWAR digits8 with exhaustive test) vs c281c15 on Granite Rapids 6975P (4 vCPU 10M):
  WIN 0.85/0.86 vs 0.91/0.93 wall; in-process 0.74 vs 0.80 (parse .355/.37, dedup .19/.24, persons .155/.165).
  7709e08 direct-index Lookup for p<n>/c_<n>; 6b789aa precomputed click lex key for the click sort. Ten seeds MATCH (8/10 in).
  Output at 10M: 12 MB (user_id dict+snappy 1.4 MB/part of 3 MB). 30M scaling run dispatched.
- 2026-11-21 ~04:30 ET: main = 6d5c18e (ten seeds MATCH). 30M on Zen3: 2.71 s wall / 2.50 in-process, linear, no OOM.
  882ead9: dedup = two passes over raw records (probe table of (key, idx) 16 B slots; no copy/sub-buckets), ~20k rec/partition
  up to 1024 partitions; TIMING=1 sub-timers. Mac thread-seconds (4 thr, 10M): event pread 1.5-1.7 (!! macOS page cache),
  scan+parse 1.1-1.2; dedup probe 0.09, emit 0.27-0.30 (of which link lookups ~0.13: NOLINK experiment); person ids 0.08,
  counting sort 0.02, sorts 0.12, walk 0.16-0.18. Events are in random time order (no pre-sorted shortcut).
  CI timing run 44880051547 (TIMING, USE_MMAP, LOG_P=8 sets). Deferred: resolve person once per distinct user (~2% at 10M,
  more at 60M); 12 B link slots (keys+u32 names) to keep the map in L3.
- 2026-11-21 ~05:30 ET: CI TIMING (SPR 8573C, 4 thr, 10M): event pread 0.10-0.13 thread-s, scan+parse 1.10 (110 ns/line!),
  link task 0.12 s on one worker; dedup probe 0.14, emit 0.31; person ids 0.10, counting 0.035, sorts 0.18, walk 0.275.
  USE_MMAP no gain; LOG_P 10 >= 8. 882ead9 ten seeds MATCH.
  Mac data-variant experiment (36 event files, 2.8M lines): stripping all quotes cut scan+parse 0.45 -> 0.30 thread-s.
  -> split_masked rewritten as ONE branch-free loop (fixed 6-iteration u128 comma walk, expected-quote-mask == qm check,
  no pext, no no-quote fast path): orig 0.34 == noq 0.33 now. Scatter staging 4 records per flush (flush branch 25%).
  all-view / all-F1 variants: within noise -> etype and ts-form branches are not major. A/B vs 6d5c18e dispatched.
- 2026-11-21 ~07:00 ET: A/B results vs main 6d5c18e (same runner each): 29c8e10 branch-free splitter LOST (parse .39 vs .355
  on GNR; Mac win was software-pext artifact). d517606 (two-pass dedup + 4-record staging, pext splitter) NEUTRAL (Zen4 9V74;
  the 1.23/1.33 walls in the first step were runner noise, later steps 0.88-0.91 == cmp). 8f1682d (v3b) confirms new splitter
  slower. x86 data variants (Zen3): no-quotes saves 10-20% of scan+parse, all-view ~0. a0b3a3d-ish batch (single pext path,
  insertion sorts <=32, link-map prefetch) NEUTRAL (0.88/0.94 vs 0.89/0.93); link task 0.115 s unchanged by prefetch.
  Two-round person stage (route by user, resolve each distinct user once, re-scatter 61% linked events by name): fixtures
  36/36, dev MATCH; CI A/B dispatched. Mac timings unusable while seedcheck's Python reference runs.
- 2026-11-21 ~07:40 ET: two-round person stage (HEAD of v3) vs main 6d5c18e on Zen4 9V74: WIN in-process 0.698/0.705 vs
  0.718/0.720, wall 0.88/0.90 vs 0.90/0.92. dedup emit 0.31 -> 0.11 thread-s; round A+B grouping 0.16 (was 0.09), sorts 0.22,
  walk 0.24. 61% of events belong to linked persons and are re-scattered. Env sweeps dispatched: UNIT_KB 128/2048, LOG_P 7/8,
  PIN=0. Parse is now ~49% of in-process time (1.08 thread-s / 10M = 108 ns/line on Zen4), the dominant remaining cost.
- 2026-11-21 ~08:30 ET: CI env sweeps (two-round build, native 4 thr 10M): UNIT_KB 128/512/2048 flat; LOG_P 7/8/10 flat;
  PIN=0 == pinned. Zen4 9V45: in-process 0.55-0.56 s, scan+parse 0.76-0.80 thread-s (76-80 ns/line); 9V74: 1.07 (slower
  clocks). Parse cost is instructions + mispredicts, not cache/TLB/scheduling. Events 65-96 B (99.8%), clicks <= 64 B
  -> mask builder exits at the chunk holding the newline (A/B vs two-round commit dispatched).
- 2026-11-21 ~09:00 ET: two-round build ten seeds MATCH (frozen binary) -> main fast-forwarded to f616eb8 (recommended).
- 2026-11-21 ~09:30 ET: c8989a5 (mask builder exits at the newline chunk) vs two-round f616eb8 on Zen4 9V74: WIN parse
  .334/.336 vs .352/.356, in-process .70/.71 vs .72, wall .92/.93 vs .93/.94; scan+parse 1.02 vs 1.07 thread-s. Seed check
  running (frozen binary); merge to main after MATCH. 30M run of main dispatched. Remaining ideas all <= 2%.
