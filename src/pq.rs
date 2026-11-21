//! Minimal Parquet writer: PLAIN encoding, no compression, data page v1, thrift compact metadata.
//! Supports INT64, INT32 as DATE, and BYTE_ARRAY as UTF8 (optionally nullable).

use std::io::Write;

pub enum Col<'a> {
    I64(&'a str, &'a [i64]),
    I32(&'a str, &'a [i32]),
    Date(&'a str, &'a [i32]),
    /// strings as (offsets, bytes); offsets.len() == n + 1; `valid` None = all present
    Str(&'a str, &'a [u32], &'a [u8], Option<&'a [bool]>),
    /// strings with dictionary encoding (RLE_DICTIONARY): best when consecutive rows repeat values
    DictStr(&'a str, &'a [u32], &'a [u8]),
}

const PAGE_ROWS: usize = 1 << 16;

struct Thrift(Vec<u8>);
impl Thrift {
    fn varint(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.0.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.0.push(v as u8);
    }
    fn zig32(&mut self, v: i32) {
        self.varint(((v << 1) ^ (v >> 31)) as u32 as u64);
    }
    fn zig64(&mut self, v: i64) {
        self.varint(((v << 1) ^ (v >> 63)) as u64);
    }
    /// field header with delta from previous id
    fn field(&mut self, last: &mut i16, id: i16, ty: u8) {
        let delta = id - *last;
        if delta > 0 && delta < 16 {
            self.0.push(((delta as u8) << 4) | ty);
        } else {
            self.0.push(ty);
            self.zig32(id as i32);
        }
        *last = id;
    }
    fn i32f(&mut self, last: &mut i16, id: i16, v: i32) {
        self.field(last, id, 5);
        self.zig32(v);
    }
    fn i64f(&mut self, last: &mut i16, id: i16, v: i64) {
        self.field(last, id, 6);
        self.zig64(v);
    }
    fn strf(&mut self, last: &mut i16, id: i16, s: &[u8]) {
        self.field(last, id, 8);
        self.varint(s.len() as u64);
        self.0.extend_from_slice(s);
    }
    fn list_begin(&mut self, last: &mut i16, id: i16, n: usize, elem_ty: u8) {
        self.field(last, id, 9);
        if n < 15 {
            self.0.push(((n as u8) << 4) | elem_ty);
        } else {
            self.0.push(0xF0 | elem_ty);
            self.varint(n as u64);
        }
    }
    fn struct_begin(&mut self, last: &mut i16, id: i16) {
        self.field(last, id, 12);
    }
    fn stop(&mut self) {
        self.0.push(0);
    }
}

fn page_header(num_values: usize, uncompressed: usize, compressed: usize) -> Vec<u8> {
    data_page_header(num_values, uncompressed, compressed, 0)
}

/// Data page v1 header; `encoding` 0 = PLAIN, 8 = RLE_DICTIONARY.
fn data_page_header(num_values: usize, uncompressed: usize, compressed: usize, encoding: i32) -> Vec<u8> {
    let mut t = Thrift(Vec::with_capacity(32));
    let mut l = 0i16;
    t.i32f(&mut l, 1, 0); // DATA_PAGE
    t.i32f(&mut l, 2, uncompressed as i32);
    t.i32f(&mut l, 3, compressed as i32);
    t.struct_begin(&mut l, 5);
    {
        let mut l2 = 0i16;
        t.i32f(&mut l2, 1, num_values as i32);
        t.i32f(&mut l2, 2, encoding);
        t.i32f(&mut l2, 3, 3); // RLE
        t.i32f(&mut l2, 4, 3); // RLE
        t.stop();
    }
    t.stop();
    t.0
}

/// Dictionary page header (PLAIN-encoded dictionary values).
fn dict_page_header(num_values: usize, uncompressed: usize, compressed: usize) -> Vec<u8> {
    let mut t = Thrift(Vec::with_capacity(32));
    let mut l = 0i16;
    t.i32f(&mut l, 1, 2); // DICTIONARY_PAGE
    t.i32f(&mut l, 2, uncompressed as i32);
    t.i32f(&mut l, 3, compressed as i32);
    t.struct_begin(&mut l, 7);
    {
        let mut l2 = 0i16;
        t.i32f(&mut l2, 1, num_values as i32);
        t.i32f(&mut l2, 2, 2); // PLAIN_DICTIONARY (the dictionary page itself is PLAIN)
        t.stop();
    }
    t.stop();
    t.0
}

/// RLE/bit-packed hybrid of dictionary indices for a data page: 1-byte bit width, then bit-packed groups of 8.
fn dict_indices(idx: &[u32], width: u32) -> Vec<u8> {
    let n = idx.len();
    let groups = (n + 7) / 8;
    let mut out = Vec::with_capacity(2 + groups * width as usize + 8);
    out.push(width as u8);
    let mut v = ((groups as u64) << 1) | 1;
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
    // pack `width` bits per value, LSB first, 8 values per group -> width bytes per group
    let mut acc: u64 = 0;
    let mut nbits = 0u32;
    for g in 0..groups {
        for b in 0..8 {
            let i = g * 8 + b;
            let val = if i < n { idx[i] as u64 } else { 0 };
            acc |= val << nbits;
            nbits += width;
            while nbits >= 8 {
                out.push(acc as u8);
                acc >>= 8;
                nbits -= 8;
            }
        }
    }
    out
}

/// Definition levels (bit width 1) as RLE/bit-packed hybrid with the v1 4-byte length prefix.
fn def_levels(valid: &[bool]) -> Vec<u8> {
    let n = valid.len();
    let groups = (n + 7) / 8;
    let mut body = Vec::with_capacity(groups + 8);
    // bit-packed run header: (groups << 1) | 1
    let mut hdr = Vec::new();
    let mut v = ((groups as u64) << 1) | 1;
    while v >= 0x80 {
        hdr.push((v as u8) | 0x80);
        v >>= 7;
    }
    hdr.push(v as u8);
    body.extend_from_slice(&hdr);
    for g in 0..groups {
        let mut byte = 0u8;
        for b in 0..8 {
            let i = g * 8 + b;
            if i < n && valid[i] {
                byte |= 1 << b;
            }
        }
        body.push(byte);
    }
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

struct ChunkMeta {
    ty: i32,
    name: String,
    num_values: i64,
    bytes: i64,
    raw_bytes: i64,
    data_page_offset: i64,
    dict_page_offset: Option<i64>,
}

pub fn write(path: &std::path::Path, cols: &[Col], nrows: usize) -> std::io::Result<()> {
    write_opts(path, cols, nrows, true)
}

pub fn write_opts(path: &std::path::Path, cols: &[Col], nrows: usize, snappy: bool) -> std::io::Result<()> {
    let mut enc = snap::raw::Encoder::new();
    let mut f = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(path)?);
    let mut pos: u64 = 0;
    f.write_all(b"PAR1")?;
    pos += 4;
    let mut metas: Vec<ChunkMeta> = Vec::new();
    for c in cols {
        let start = pos;
        let mut total = 0u64;
        let mut raw_total = 0u64;
        let (ty, name) = match c {
            Col::I64(n, _) => (2, *n),
            Col::I32(n, _) => (1, *n),
            Col::Date(n, _) => (1, *n),
            Col::Str(n, _, _, _) => (6, *n),
            Col::DictStr(n, _, _) => (6, *n),
        };
        let mut dict_page_offset = None;
        let mut first_data_page: Option<u64> = None;
        if let Col::DictStr(_, offs, bytes) = c {
            // build the dictionary over the whole chunk (consecutive duplicates are the common case)
            let mut dict_bytes: Vec<u8> = Vec::with_capacity(bytes.len() / 2 + 16);
            let mut ids: Vec<u32> = Vec::with_capacity(nrows);
            let mut map: std::collections::HashMap<&[u8], u32> = std::collections::HashMap::with_capacity(nrows / 2 + 16);
            let mut last: Option<(&[u8], u32)> = None;
            for i in 0..nrows {
                let sl = &bytes[offs[i] as usize..offs[i + 1] as usize];
                let id = match last {
                    Some((p, id)) if p == sl => id,
                    _ => {
                        let next = map.len() as u32;
                        let id = *map.entry(sl).or_insert_with(|| {
                            dict_bytes.extend_from_slice(&(sl.len() as u32).to_le_bytes());
                            dict_bytes.extend_from_slice(sl);
                            next
                        });
                        last = Some((sl, id));
                        id
                    }
                };
                ids.push(id);
            }
            let ndict = map.len();
            let width = if ndict <= 1 { 1 } else { 32 - (ndict as u32 - 1).leading_zeros() };
            // dictionary page
            let (hdr, body) = if snappy {
                let cmp = enc.compress_vec(&dict_bytes).expect("snappy");
                (dict_page_header(ndict, dict_bytes.len(), cmp.len()), cmp)
            } else {
                (dict_page_header(ndict, dict_bytes.len(), dict_bytes.len()), dict_bytes.clone())
            };
            dict_page_offset = Some(pos as i64);
            f.write_all(&hdr)?;
            f.write_all(&body)?;
            total += (hdr.len() + body.len()) as u64;
            raw_total += (hdr.len() + dict_bytes.len()) as u64;
            // data pages of indices
            let mut row = 0;
            while row < nrows || (nrows == 0 && row == 0) {
                let end = (row + PAGE_ROWS).min(nrows);
                let page = dict_indices(&ids[row..end], width);
                let ulen = page.len();
                let (hdr, body) = if snappy {
                    let cmp = enc.compress_vec(&page).expect("snappy");
                    (data_page_header(end - row, ulen, cmp.len(), 8), cmp)
                } else {
                    (data_page_header(end - row, ulen, ulen, 8), page)
                };
                if first_data_page.is_none() {
                    first_data_page = Some(pos + total);
                }
                f.write_all(&hdr)?;
                f.write_all(&body)?;
                total += (hdr.len() + body.len()) as u64;
                raw_total += (hdr.len() + ulen) as u64;
                row = end;
                if nrows == 0 {
                    break;
                }
            }
            pos += total;
            metas.push(ChunkMeta {
                ty,
                name: name.to_string(),
                num_values: nrows as i64,
                bytes: total as i64,
                raw_bytes: raw_total as i64,
                data_page_offset: first_data_page.unwrap_or(start) as i64,
                dict_page_offset,
            });
            continue;
        }
        let mut row = 0;
        while row < nrows || (nrows == 0 && row == 0) {
            let end = (row + PAGE_ROWS).min(nrows);
            let mut page: Vec<u8> = Vec::new();
            match c {
                Col::I64(_, v) => {
                    for x in &v[row..end] {
                        page.extend_from_slice(&x.to_le_bytes());
                    }
                }
                Col::Date(_, v) | Col::I32(_, v) => {
                    for x in &v[row..end] {
                        page.extend_from_slice(&x.to_le_bytes());
                    }
                }
                Col::DictStr(..) => unreachable!(),
                Col::Str(_, offs, bytes, valid) => {
                    if let Some(valid) = valid {
                        page.extend_from_slice(&def_levels(&valid[row..end]));
                    }
                    for i in row..end {
                        if valid.map_or(true, |v| v[i]) {
                            let (a, b) = (offs[i] as usize, offs[i + 1] as usize);
                            page.extend_from_slice(&((b - a) as u32).to_le_bytes());
                            page.extend_from_slice(&bytes[a..b]);
                        }
                    }
                }
            }
            let uncompressed_len = page.len();
            let (hdr, body) = if snappy {
                let c = enc.compress_vec(&page).expect("snappy");
                (page_header(end - row, page.len(), c.len()), c)
            } else {
                (page_header(end - row, page.len(), page.len()), page)
            };
            f.write_all(&hdr)?;
            f.write_all(&body)?;
            total += (hdr.len() + body.len()) as u64;
            raw_total += (hdr.len() + uncompressed_len) as u64;
            row = end;
            if nrows == 0 {
                break;
            }
        }
        pos += total;
        metas.push(ChunkMeta { ty, name: name.to_string(), num_values: nrows as i64, bytes: total as i64, raw_bytes: raw_total as i64, data_page_offset: start as i64, dict_page_offset });
    }
    // FileMetaData
    let mut t = Thrift(Vec::with_capacity(512));
    let mut l = 0i16;
    t.i32f(&mut l, 1, 1); // version
    t.list_begin(&mut l, 2, cols.len() + 1, 12);
    {
        // root
        let mut l2 = 0i16;
        t.strf(&mut l2, 4, b"schema");
        t.i32f(&mut l2, 5, cols.len() as i32);
        t.stop();
        for c in cols {
            let mut l2 = 0i16;
            match c {
                Col::I64(n, _) => {
                    t.i32f(&mut l2, 1, 2);
                    t.i32f(&mut l2, 3, 0);
                    t.strf(&mut l2, 4, n.as_bytes());
                }
                Col::Date(n, _) => {
                    t.i32f(&mut l2, 1, 1);
                    t.i32f(&mut l2, 3, 0);
                    t.strf(&mut l2, 4, n.as_bytes());
                    t.i32f(&mut l2, 6, 6); // DATE
                }
                Col::I32(n, _) => {
                    t.i32f(&mut l2, 1, 1);
                    t.i32f(&mut l2, 3, 0);
                    t.strf(&mut l2, 4, n.as_bytes());
                }
                Col::DictStr(n, _, _) => {
                    t.i32f(&mut l2, 1, 6);
                    t.i32f(&mut l2, 3, 0);
                    t.strf(&mut l2, 4, n.as_bytes());
                    t.i32f(&mut l2, 6, 0); // UTF8
                }
                Col::Str(n, _, _, valid) => {
                    t.i32f(&mut l2, 1, 6);
                    t.i32f(&mut l2, 3, if valid.is_some() { 1 } else { 0 });
                    t.strf(&mut l2, 4, n.as_bytes());
                    t.i32f(&mut l2, 6, 0); // UTF8
                }
            }
            t.stop();
        }
    }
    t.i64f(&mut l, 3, nrows as i64);
    t.list_begin(&mut l, 4, 1, 12);
    {
        let mut l2 = 0i16;
        t.list_begin(&mut l2, 1, metas.len(), 12);
        let mut total_bytes = 0i64;
        for m in &metas {
            let mut l3 = 0i16;
            t.i64f(&mut l3, 2, m.data_page_offset);
            t.struct_begin(&mut l3, 3);
            {
                let mut l4 = 0i16;
                t.i32f(&mut l4, 1, m.ty);
                if m.dict_page_offset.is_some() {
                    t.list_begin(&mut l4, 2, 3, 5);
                    t.zig32(2); // PLAIN_DICTIONARY (dictionary page)
                    t.zig32(8); // RLE_DICTIONARY (data pages)
                    t.zig32(3); // RLE (levels)
                } else {
                    t.list_begin(&mut l4, 2, 2, 5);
                    t.zig32(0); // PLAIN
                    t.zig32(3); // RLE
                }
                t.list_begin(&mut l4, 3, 1, 8);
                t.varint(m.name.len() as u64);
                t.0.extend_from_slice(m.name.as_bytes());
                t.i32f(&mut l4, 4, if snappy { 1 } else { 0 }); // SNAPPY / UNCOMPRESSED
                t.i64f(&mut l4, 5, m.num_values);
                t.i64f(&mut l4, 6, m.raw_bytes);
                t.i64f(&mut l4, 7, m.bytes);
                t.i64f(&mut l4, 9, m.data_page_offset);
                if let Some(d) = m.dict_page_offset {
                    t.i64f(&mut l4, 11, d);
                }
                t.stop();
            }
            t.stop();
            total_bytes += m.bytes;
        }
        t.i64f(&mut l2, 2, total_bytes);
        t.i64f(&mut l2, 3, nrows as i64);
        t.stop();
    }
    t.strf(&mut l, 6, b"shopsess");
    t.stop();
    f.write_all(&t.0)?;
    f.write_all(&(t.0.len() as u32).to_le_bytes())?;
    f.write_all(b"PAR1")?;
    f.flush()
}
