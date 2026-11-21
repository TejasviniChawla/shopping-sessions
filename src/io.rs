//! Input discovery, mmapping, and splitting files into newline-aligned work units.

use memmap2::Mmap;
use std::fs::File;
use std::path::{Path, PathBuf};

pub struct Inputs {
    pub files: Vec<Mmap>,
    /// Work units: (file index, start, end) with start after the header line; end at a line boundary.
    pub events: Vec<(usize, usize, usize)>,
    pub clicks: Vec<(usize, usize, usize)>,
    pub links: Vec<(usize, usize, usize)>,
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

const UNIT: usize = 4 << 20;

/// Opens every CSV in the folder, mmaps it, and appends work units. Header line is skipped.
fn add_folder(dir: &Path, files: &mut Vec<Mmap>, units: &mut Vec<(usize, usize, usize)>) {
    for p in list_csv(dir) {
        let f = match File::open(&p) {
            Ok(f) => f,
            Err(_) => continue,
        };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0) as usize;
        if len == 0 {
            continue;
        }
        let m = unsafe { Mmap::map(&f) }.expect("mmap");
        #[cfg(target_os = "linux")]
        let _ = m.advise(memmap2::Advice::Sequential);
        let idx = files.len();
        // skip header
        let hdr_end = match memchr::memchr(b'\n', &m[..]) {
            Some(k) => k + 1,
            None => len, // header only, no newline
        };
        let mut start = hdr_end;
        while start < len {
            let mut end = (start + UNIT).min(len);
            if end < len {
                match memchr::memchr(b'\n', &m[end..]) {
                    Some(k) => end = end + k + 1,
                    None => end = len,
                }
            }
            units.push((idx, start, end));
            start = end;
        }
        files.push(m);
    }
}

pub fn discover(data: &Path) -> Inputs {
    let mut files = Vec::new();
    let mut events = Vec::new();
    let mut clicks = Vec::new();
    let mut links = Vec::new();
    add_folder(&data.join("events"), &mut files, &mut events);
    add_folder(&data.join("clicks"), &mut files, &mut clicks);
    add_folder(&data.join("links"), &mut files, &mut links);
    // Largest units first for better load balance.
    events.sort_by_key(|u| std::cmp::Reverse(u.2 - u.1));
    clicks.sort_by_key(|u| std::cmp::Reverse(u.2 - u.1));
    links.sort_by_key(|u| std::cmp::Reverse(u.2 - u.1));
    Inputs { files, events, clicks, links }
}
