use time::{format_description, OffsetDateTime};

pub fn datetime_from_millis(millis: u64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp_nanos((millis as i128) * 1_000_000)
        .expect("timestamp out of range")
}

pub fn date_from_datetime(dt: OffsetDateTime) -> String {
    dt.format(&format_description::parse("[year][month][day]").unwrap()).unwrap()
}
