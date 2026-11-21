# Prior-art research (compiled 2026-11-20)

Budget reality: `docker run` overhead on Linux is ~0.3-0.5 s regardless of image size
(arxiv 2602.15214, iron.io, jvns). A 1 s score => ~0.5 s of in-container work.
Rough budget: 0.3-0.4 s parse, <=0.1 s dedup/intern, <=0.05 s CC, <=0.1 s sessionize+attribute, <=0.05 s Parquet.

## CSV parsing
- csveee (Ellmann & Neumann, PVLDB 19(11) 2026) https://www.vldb.org/pvldb/vol19/p3579-ellmann.pdf
  Rust, 1.8 GB/s/core quoted. 64-byte bitmasks for , \n ", quote mask via pclmulqdq prefix-XOR,
  tzcnt loop for sparse masks beats simdjson's 8-at-a-time. Speculative chunk starts, verify on merge.
  I/O: per-thread read() into reused buffers 183 GB/s vs shared mmap 34 GB/s (page faults/TLB). MAP_POPULATE hurts.
- simdjson (Langdale & Lemire) https://arxiv.org/abs/1902.08318 : stage-1 structural indexing techniques.
- Ge/Chang et al. SIGMOD 2019 speculative CSV chunking: speculate quote parity on first 64 KB, mispredict <1e-7.
- Cascade Lake has AVX-512BW/VL but NOT VBMI2 (no vpcompressb). vpcompressd (AVX-512F) works.
- Rust `csv` crate: 0.7 GB/s/core, too slow; use `memchr` (runtime AVX2 dispatch) for separators.

## Integer / timestamp parsing
- SWAR 8-digit parse (Lemire): val -= 0x3030..; val = val*10 + (val>>8); val = ((val&M)*mul1 + ((val>>16)&M)*mul2)>>32
  M=0x000000FF000000FF mul1=0x000F424000000064 mul2=0x0000271000000001
- Branchless is-8-digits: ((v & 0xF0F0..) | (((v + 0x0606..) & 0xF0F0..) >> 4)) == 0x3333..
- Lemire SIMD timestamp validate+parse: ~8 ns/stamp https://lemire.me/blog/2023/07/01/parsing-time-stamps-faster-with-simd-instructions/
- days_from_civil (Hinnant); Neri-Schneider EAF ~2x faster. For a 14-day window a LUT by (year,month) is cheapest.

## Connected components
- ConnectIt (PVLDB 2021): UF-Rem-CAS with splitting/halving wins. Afforest k=2 sampling + skip largest component.
- Alistarh et al 2019: halving ~= splitting, both beat full compression; plain writes beat CAS where safe.
- Practical: intern IDs to dense u32, sequential Rem/union-by-index + path halving on Vec<u32>: 1-5M edges in 10-40 ms.

## Sessionize / attribution
- No papers; standard = sort (key, ts) then linear scan. Use counting sort by dense person id, then small in-cache sort by ts.
- Last-touch 7-day lookback = ASOF join; both sides sorted per person => one merge walk does credit + assists.
- Aggregate into dense arrays indexed (id*n_days+day).

## Parquet
- Snappy/LZ4 ~free, ZSTD +13%, GZIP +57%. Write UNCOMPRESSED or SNAPPY. Tables are tiny here anyway.
- arrow-rs `parquet` ArrowColumnWriter for parallel column encode; not needed at this size.

## Process hygiene
- Static binary, FROM scratch. musl malloc slow -> mimalloc or gnu +crt-static.
- 1BRC tricks: zero per-row alloc, atomic-counter work units, open-addressing on raw bytes,
  fork trick so parent exits while kernel tears down child's mappings (munmap of multi-GB is on the clock).
- munmap cost: ~125 ms/GB of 4K page faults if touched single-threaded. read() into reused buffers avoids both.
- -C target-cpu=cascadelake, lto=fat, codegen-units=1, panic=abort. PGO 5-15%.

## 1BRC references
- morling.dev results; thomaswue winner source; QuestDB step-by-step (71 s -> 1.7 s, 2032 -> 98 instr/row);
  lehuyduc 1brc-simd (C++); ayebear/nibtr Rust ports.

Full extracted paper texts in scratchpad/pdf/*.txt (session-local).
