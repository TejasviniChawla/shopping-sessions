//! Parquet output of the two tables via the minimal PLAIN writer in `pq.rs`.

use crate::pq::{self, Col};
use std::path::Path;

/// Columnar buffers for user_daily rows; strings are stored as (offsets, bytes).
pub struct UserDailyCols {
    pub uid_offs: Vec<u32>,
    pub uid_bytes: Vec<u8>,
    pub day: Vec<i32>,
    pub sessions: Vec<i32>,
    pub events: Vec<i32>,
    pub revenue: Vec<i64>,
    pub cat_offs: Vec<u32>,
    pub cat_bytes: Vec<u8>,
    pub cat_valid: Vec<bool>,
}

impl UserDailyCols {
    pub fn new(n: usize) -> Self {
        let mut uid_offs = Vec::with_capacity(n + 1);
        uid_offs.push(0);
        let mut cat_offs = Vec::with_capacity(n + 1);
        cat_offs.push(0);
        UserDailyCols {
            uid_offs,
            uid_bytes: Vec::with_capacity(n * 10),
            day: Vec::with_capacity(n),
            sessions: Vec::with_capacity(n),
            events: Vec::with_capacity(n),
            revenue: Vec::with_capacity(n),
            cat_offs,
            cat_bytes: Vec::with_capacity(n * 2),
            cat_valid: Vec::with_capacity(n),
        }
    }
    #[inline(always)]
    pub fn push_user(&mut self, b: &[u8]) {
        self.uid_bytes.extend_from_slice(b);
        self.uid_offs.push(self.uid_bytes.len() as u32);
    }
    #[inline(always)]
    pub fn push_cat(&mut self, b: Option<&[u8]>) {
        if let Some(b) = b {
            self.cat_bytes.extend_from_slice(b);
            self.cat_valid.push(true);
        } else {
            self.cat_valid.push(false);
        }
        self.cat_offs.push(self.cat_bytes.len() as u32);
    }
    pub fn len(&self) -> usize {
        self.day.len()
    }
}

pub fn write_user_daily(path: &Path, c: UserDailyCols) -> std::io::Result<()> {
    let cols = [
        Col::DictStr("user_id", &c.uid_offs, &c.uid_bytes),
        Col::Date("day", &c.day),
        Col::I32("sessions", &c.sessions),
        Col::I32("events", &c.events),
        Col::I64("revenue_cents", &c.revenue),
        Col::Str("top_category", &c.cat_offs, &c.cat_bytes, Some(&c.cat_valid)),
    ];
    pq::write(path, &cols, c.len())
}

pub struct CampaignDailyCols {
    pub cid_offs: Vec<u32>,
    pub cid_bytes: Vec<u8>,
    pub ch_offs: Vec<u32>,
    pub ch_bytes: Vec<u8>,
    pub day: Vec<i32>,
    pub sessions: Vec<i32>,
    pub purchasing_sessions: Vec<i32>,
    pub revenue: Vec<i64>,
    pub assisted_sessions: Vec<i32>,
}

impl CampaignDailyCols {
    pub fn new(n: usize) -> Self {
        CampaignDailyCols {
            cid_offs: vec![0],
            cid_bytes: Vec::with_capacity(n * 6),
            ch_offs: vec![0],
            ch_bytes: Vec::with_capacity(n * 7),
            day: Vec::with_capacity(n),
            sessions: Vec::with_capacity(n),
            purchasing_sessions: Vec::with_capacity(n),
            revenue: Vec::with_capacity(n),
            assisted_sessions: Vec::with_capacity(n),
        }
    }
    #[inline(always)]
    pub fn push_ids(&mut self, cid: &[u8], ch: &[u8]) {
        self.cid_bytes.extend_from_slice(cid);
        self.cid_offs.push(self.cid_bytes.len() as u32);
        self.ch_bytes.extend_from_slice(ch);
        self.ch_offs.push(self.ch_bytes.len() as u32);
    }
}

pub fn write_campaign_daily(dir: &Path, c: CampaignDailyCols) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let cols = [
        Col::DictStr("campaign_id", &c.cid_offs, &c.cid_bytes),
        Col::DictStr("channel", &c.ch_offs, &c.ch_bytes),
        Col::Date("day", &c.day),
        Col::I32("sessions", &c.sessions),
        Col::I32("purchasing_sessions", &c.purchasing_sessions),
        Col::I64("revenue_cents", &c.revenue),
        Col::I32("assisted_sessions", &c.assisted_sessions),
    ];
    pq::write(&dir.join("part-0.parquet"), &cols, c.day.len())
}
