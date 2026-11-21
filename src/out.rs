//! Parquet output of the two tables.

use arrow_array::builder::StringBuilder;
use arrow_array::{ArrayRef, Date32Array, Int64Array, RecordBatch};
use arrow_schema::{DataType, Field, Schema};
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

fn props() -> WriterProperties {
    WriterProperties::builder()
        .set_compression(Compression::SNAPPY)
        .set_max_row_group_row_count(Some(1 << 20))
        .set_statistics_enabled(parquet::file::properties::EnabledStatistics::None)
        .build()
}

pub struct UserDailyCols {
    pub user_id: StringBuilder,
    pub day: Vec<i32>,
    pub sessions: Vec<i64>,
    pub events: Vec<i64>,
    pub revenue: Vec<i64>,
    pub top_category: StringBuilder,
}

impl UserDailyCols {
    pub fn new(n: usize) -> Self {
        UserDailyCols {
            user_id: StringBuilder::with_capacity(n, n * 10),
            day: Vec::with_capacity(n),
            sessions: Vec::with_capacity(n),
            events: Vec::with_capacity(n),
            revenue: Vec::with_capacity(n),
            top_category: StringBuilder::with_capacity(n, n * 4),
        }
    }
}

pub fn write_user_daily(dir: &Path, mut c: UserDailyCols) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let schema = Arc::new(Schema::new(vec![
        Field::new("user_id", DataType::Utf8, false),
        Field::new("day", DataType::Date32, false),
        Field::new("sessions", DataType::Int64, false),
        Field::new("events", DataType::Int64, false),
        Field::new("revenue_cents", DataType::Int64, false),
        Field::new("top_category", DataType::Utf8, true),
    ]));
    let cols: Vec<ArrayRef> = vec![
        Arc::new(c.user_id.finish()),
        Arc::new(Date32Array::from(c.day)),
        Arc::new(Int64Array::from(c.sessions)),
        Arc::new(Int64Array::from(c.events)),
        Arc::new(Int64Array::from(c.revenue)),
        Arc::new(c.top_category.finish()),
    ];
    let batch = RecordBatch::try_new(schema.clone(), cols).expect("batch");
    let file = File::create(dir.join("part-0.parquet"))?;
    let mut w = ArrowWriter::try_new(file, schema, Some(props())).expect("writer");
    w.write(&batch).expect("write");
    w.close().expect("close");
    Ok(())
}

pub struct CampaignDailyCols {
    pub campaign_id: StringBuilder,
    pub channel: StringBuilder,
    pub day: Vec<i32>,
    pub sessions: Vec<i64>,
    pub purchasing_sessions: Vec<i64>,
    pub revenue: Vec<i64>,
    pub assisted_sessions: Vec<i64>,
}

impl CampaignDailyCols {
    pub fn new(n: usize) -> Self {
        CampaignDailyCols {
            campaign_id: StringBuilder::with_capacity(n, n * 6),
            channel: StringBuilder::with_capacity(n, n * 7),
            day: Vec::with_capacity(n),
            sessions: Vec::with_capacity(n),
            purchasing_sessions: Vec::with_capacity(n),
            revenue: Vec::with_capacity(n),
            assisted_sessions: Vec::with_capacity(n),
        }
    }
}

pub fn write_campaign_daily(dir: &Path, mut c: CampaignDailyCols) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let schema = Arc::new(Schema::new(vec![
        Field::new("campaign_id", DataType::Utf8, false),
        Field::new("channel", DataType::Utf8, false),
        Field::new("day", DataType::Date32, false),
        Field::new("sessions", DataType::Int64, false),
        Field::new("purchasing_sessions", DataType::Int64, false),
        Field::new("revenue_cents", DataType::Int64, false),
        Field::new("assisted_sessions", DataType::Int64, false),
    ]));
    let cols: Vec<ArrayRef> = vec![
        Arc::new(c.campaign_id.finish()),
        Arc::new(c.channel.finish()),
        Arc::new(Date32Array::from(c.day)),
        Arc::new(Int64Array::from(c.sessions)),
        Arc::new(Int64Array::from(c.purchasing_sessions)),
        Arc::new(Int64Array::from(c.revenue)),
        Arc::new(Int64Array::from(c.assisted_sessions)),
    ];
    let batch = RecordBatch::try_new(schema.clone(), cols).expect("batch");
    let file = File::create(dir.join("part-0.parquet"))?;
    let mut w = ArrowWriter::try_new(file, schema, Some(props())).expect("writer");
    w.write(&batch).expect("write");
    w.close().expect("close");
    Ok(())
}
