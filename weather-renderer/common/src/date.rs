use chrono::{Datelike, Local, NaiveDate, Weekday};

pub fn local_today() -> NaiveDate {
    Local::now().date_naive()
}

pub fn multi_day_date_label(value: &str, today: NaiveDate) -> String {
    let input = value.trim();
    if input.is_empty() {
        return "-".to_string();
    }
    parse_calendar_date(input, today)
        .map_or_else(|| input.to_string(), |date| label_for_date(date, today))
}

pub fn multi_day_datetime_label(value: &str, today: NaiveDate) -> String {
    let input = value.trim();
    if input.is_empty() {
        return "-".to_string();
    }
    let Some((date, token_len)) = parse_calendar_date_with_len(input, today) else {
        return input.to_string();
    };
    let suffix = input[token_len..].trim_start_matches(['T', ' ']);
    let time = suffix
        .split_whitespace()
        .next()
        .filter(|value| value.contains(':'));
    match time {
        Some(time) => format!("{} {time}", label_for_date(date, today)),
        None => label_for_date(date, today),
    }
}

fn parse_calendar_date(value: &str, today: NaiveDate) -> Option<NaiveDate> {
    parse_calendar_date_with_len(value, today).map(|(date, _)| date)
}

fn parse_calendar_date_with_len(value: &str, today: NaiveDate) -> Option<(NaiveDate, usize)> {
    let token = value.split(['T', ' ']).next()?;
    let token_len = token.len();
    let normalized = token
        .trim_end_matches('日')
        .chars()
        .map(|character| match character {
            '年' | '月' | '/' | '.' => '-',
            other => other,
        })
        .collect::<String>();
    let parts = normalized.split('-').collect::<Vec<_>>();
    let date = match parts.as_slice() {
        [year, month, day] => {
            NaiveDate::from_ymd_opt(year.parse().ok()?, month.parse().ok()?, day.parse().ok()?)
        }
        [month, day] => {
            let month = month.parse().ok()?;
            let day = day.parse().ok()?;
            [today.year() - 1, today.year(), today.year() + 1]
                .into_iter()
                .filter_map(|year| NaiveDate::from_ymd_opt(year, month, day))
                .min_by_key(|candidate| (*candidate - today).num_days().unsigned_abs())
        }
        _ => None,
    }?;
    Some((date, token_len))
}

fn label_for_date(value: NaiveDate, today: NaiveDate) -> String {
    match (value - today).num_days() {
        -1 => "昨天".to_string(),
        0 => "今天".to_string(),
        1 => "明天".to_string(),
        2..=7 => match value.weekday() {
            Weekday::Mon => "星期一".to_string(),
            Weekday::Tue => "星期二".to_string(),
            Weekday::Wed => "星期三".to_string(),
            Weekday::Thu => "星期四".to_string(),
            Weekday::Fri => "星期五".to_string(),
            Weekday::Sat => "星期六".to_string(),
            Weekday::Sun => "星期日".to_string(),
        },
        _ => value.format("%Y-%m-%d").to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multi_day_labels_cover_relative_weekday_and_concrete_ranges() {
        let today = NaiveDate::from_ymd_opt(2026, 7, 17).unwrap();
        assert_eq!(multi_day_date_label("2026-07-15", today), "2026-07-15");
        assert_eq!(multi_day_date_label("2026-07-16", today), "昨天");
        assert_eq!(multi_day_date_label("2026-07-17", today), "今天");
        assert_eq!(multi_day_date_label("07/18", today), "明天");
        assert_eq!(multi_day_date_label("07月19日", today), "星期日");
        assert_eq!(multi_day_date_label("2026.07.24", today), "星期五");
        assert_eq!(multi_day_date_label("2026-07-25", today), "2026-07-25");
    }

    #[test]
    fn partial_dates_choose_the_nearest_year_and_datetimes_keep_the_time() {
        let year_end = NaiveDate::from_ymd_opt(2026, 12, 31).unwrap();
        assert_eq!(multi_day_date_label("01-01", year_end), "明天");
        assert_eq!(
            multi_day_datetime_label("2026-12-30 23:30:00", year_end),
            "昨天 23:30:00"
        );
        assert_eq!(multi_day_date_label("not-a-date", year_end), "not-a-date");
    }
}
