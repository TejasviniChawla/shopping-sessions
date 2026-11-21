//! Input discovery and fixed-size byte-range work units read with pread into per-thread padded buffers.
//! A line belongs to the unit holding its first byte; a unit reads a tail past its end to finish its last line.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

pub const UNIT: usize = 512 << 10;

/// Work-unit size in bytes; UNIT_KB overrides for experiments (a unit that fits L2 keeps the parse reading from L2).
pub fn unit_size() -> usize {
    std::env::var("UNIT_KB").ok().and_then(|v| v.parse::<usize>().ok()).map(|kb| kb << 10).unwrap_or(UNIT)
}
/// Bytes read past the unit end to complete the last line (grown if a line is longer).
pub const TAIL: usize = 64 << 10;
/// Zero padding after the data so SIMD may over-read.
pub const PAD: usize = 256;

#[derive(Clone, Copy)]
pub struct Unit {
    pub file: u32,
    pub start: u64,
    pub end: u64,
}

pub struct Inputs {
    pub files: Vec<File>,
    pub sizes: Vec<u64>,
    pub events: Vec<Unit>,
    pub clicks: Vec<Unit>,
    pub links: Vec<Unit>,
}

fn list_csv(dir: &Path) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().map(|x| x == "csv").unwrap_or(false) && p.is_file() {
                v.push(p);
            }
        }
    }
    v.sort();
    v
}

fn add_folder(dir: &Path, files: &mut Vec<File>, sizes: &mut Vec<u64>, units: &mut Vec<Unit>) {
    for p in list_csv(dir) {
        let f = match File::open(&p) {
            Ok(f) => f,
            Err(_) => continue,
        };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        if len == 0 {
            continue;
        }
        let idx = files.len() as u32;
        let unit = unit_size() as u64;
        let mut start = 0u64;
        while start < len {
            let end = (start + unit).min(len);
            units.push(Unit { file: idx, start, end });
            start = end;
        }
        files.push(f);
        sizes.push(len);
    }
}

pub fn discover(data: &Path) -> Inputs {
    let mut files = Vec::new();
    let mut sizes = Vec::new();
    let (mut events, mut clicks, mut links) = (Vec::new(), Vec::new(), Vec::new());
    add_folder(&data.join("events"), &mut files, &mut sizes, &mut events);
    add_folder(&data.join("clicks"), &mut files, &mut sizes, &mut clicks);
    add_folder(&data.join("links"), &mut files, &mut sizes, &mut links);
    Inputs { files, sizes, events, clicks, links }
}

/// A reusable read buffer. After `load`, `data()` holds the unit's bytes from `first_line_start`
/// up to (and including) the newline that ends the last line starting before `end`; PAD zero bytes follow.
pub struct Buffer {
    buf: Vec<u8>,
    /// Index in `buf` where the first line owned by this unit starts.
    pub lo: usize,
    /// Index in `buf` one past the last byte of the unit's data (end of last owned line, after its '\n').
    pub hi: usize,
    /// Optional mmap of the unit (USE_MMAP=1): data() then points into the mapping.
    map: *mut u8,
    map_base: *mut u8,
    map_len: usize,
    use_mmap: bool,
}

#[cfg(target_os = "linux")]
fn populate_flag() -> libc::c_int {
    libc::MAP_POPULATE
}
#[cfg(not(target_os = "linux"))]
fn populate_flag() -> libc::c_int {
    0
}

impl Drop for Buffer {
    fn drop(&mut self) {
        self.unmap();
    }
}

impl Buffer {
    pub fn new() -> Self {
        let use_mmap = std::env::var_os("USE_MMAP").is_some();
        Buffer { buf: vec![0u8; unit_size() + TAIL + PAD + 1], lo: 0, hi: 0, map: std::ptr::null_mut(), map_base: std::ptr::null_mut(), map_len: 0, use_mmap }
    }

    #[inline(always)]
    pub fn data(&self) -> &[u8] {
        if self.map.is_null() {
            &self.buf[..self.hi]
        } else {
            unsafe { std::slice::from_raw_parts(self.map, self.hi) }
        }
    }

    fn unmap(&mut self) {
        if !self.map.is_null() {
            unsafe { libc::munmap(self.map_base as *mut libc::c_void, self.map_len) };
            self.map = std::ptr::null_mut();
            self.map_len = 0;
        }
    }

    pub fn load(&mut self, file: &File, size: u64, u: Unit) {
        self.unmap();
        // mmap path: only for units whose tail (plus SIMD over-read) stays inside the file.
        if self.use_mmap && u.end + (TAIL + PAD) as u64 <= size {
            use std::os::unix::io::AsRawFd;
            let page = 4096u64;
            let rstart = u.start.saturating_sub(1);
            let mstart = rstart / page * page;
            let mend = u.end + TAIL as u64 + PAD as u64;
            let len = (mend - mstart) as usize;
            let p = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    len,
                    libc::PROT_READ,
                    libc::MAP_PRIVATE | if std::env::var_os("MAP_NOPOPULATE").is_some() { 0 } else { populate_flag() },
                    file.as_raw_fd(),
                    mstart as i64,
                )
            };
            if p != libc::MAP_FAILED {
                let base = unsafe { (p as *mut u8).add((rstart - mstart) as usize) };
                let data_len = (u.end + TAIL as u64 - rstart) as usize;
                let d = unsafe { std::slice::from_raw_parts(base, data_len) };
                let unit_off = (u.start - rstart) as usize;
                let mut lo = if u.start == 0 {
                    match memchr::memchr(b'\n', d) {
                        Some(k) => k + 1,
                        None => data_len,
                    }
                } else if d[0] == b'\n' {
                    unit_off
                } else {
                    match memchr::memchr(b'\n', &d[unit_off..]) {
                        Some(k) => unit_off + k + 1,
                        None => data_len,
                    }
                };
                let end_off = (u.end - rstart) as usize;
                if lo > end_off {
                    lo = end_off;
                }
                let hi = if end_off == 0 || end_off <= lo {
                    lo
                } else if d[end_off - 1] == b'\n' {
                    end_off
                } else {
                    match memchr::memchr(b'\n', &d[end_off..]) {
                        Some(k) => end_off + k + 1,
                        None => usize::MAX, // line longer than TAIL: fall back to the copy path
                    }
                };
                if hi != usize::MAX {
                    self.map = base;
                    self.map_len = len;
                    // keep the mapping base for munmap: store the page-aligned pointer separately
                    self.map_base = p as *mut u8;
                    self.lo = lo;
                    self.hi = hi;
                    return;
                }
                unsafe { libc::munmap(p, len) };
            }
        }
        // Read from start-1 so the byte before the unit tells us whether `start` begins a line.
        let rstart = u.start.saturating_sub(1);
        let mut want = (u.end + TAIL as u64).min(size) - rstart;
        loop {
            let need = want as usize + PAD;
            if self.buf.len() < need {
                self.buf.resize(need, 0);
            }
            file.read_exact_at(&mut self.buf[..want as usize], rstart).expect("read");
            // zero the padding
            for b in &mut self.buf[want as usize..want as usize + PAD] {
                *b = 0;
            }
            let data_len = want as usize;
            // first owned line start
            let unit_off = (u.start - rstart) as usize; // 0 or 1
            let mut lo = if u.start == 0 {
                // skip header line
                match memchr::memchr(b'\n', &self.buf[..data_len]) {
                    Some(k) => k + 1,
                    None => data_len,
                }
            } else if self.buf[0] == b'\n' {
                unit_off
            } else {
                match memchr::memchr(b'\n', &self.buf[unit_off..data_len]) {
                    Some(k) => unit_off + k + 1,
                    None => data_len,
                }
            };
            let end_off = (u.end - rstart) as usize;
            if lo > end_off {
                lo = end_off;
            }
            // find the end of the last line that starts before end_off: first '\n' at or after end_off-1... precisely:
            // lines starting before end_off; the last such line ends at the first '\n' at index >= end_off-1 if
            // byte end_off-1 is not itself a '\n' terminating a previous line... simpler: the unit owns all bytes
            // in [lo, end_off) plus the continuation of the line containing byte end_off-1 (if that byte isn't '\n').
            let hi;
            if end_off == 0 || end_off <= lo {
                hi = lo;
            } else if self.buf[end_off - 1] == b'\n' {
                hi = end_off;
            } else {
                match memchr::memchr(b'\n', &self.buf[end_off..data_len]) {
                    Some(k) => hi = end_off + k + 1,
                    None => {
                        if rstart + want >= size {
                            hi = data_len; // last line of the file without trailing newline
                        } else {
                            // line longer than TAIL: read more
                            want = (want * 2).min(size - rstart);
                            continue;
                        }
                    }
                }
            }
            self.lo = lo;
            self.hi = hi;
            return;
        }
    }
}
