use std::path::Path;
use time::{macros::format_description, Date, OffsetDateTime, Time};

pub struct TimeUtils;

impl TimeUtils {
    pub fn datetime_from_path(path: &Path) -> Option<OffsetDateTime> {
        let (dt_part, hour_part) =
            (path.parent()?.file_name()?.to_str()?, path.file_name()?.to_str()?);
        Some(OffsetDateTime::new_utc(
            Date::parse(dt_part, &format_description!("[year][month][day]")).ok()?,
            Time::from_hms(hour_part.parse().ok()?, 0, 0).ok()?,
        ))
    }

    pub fn date_from_datetime(dt: OffsetDateTime) -> String {
        dt.format(&format_description!("[year][month][day]")).unwrap()
    }
}
