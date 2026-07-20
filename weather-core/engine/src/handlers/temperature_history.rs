use std::collections::BTreeMap;

use chrono::{Datelike, NaiveDate};
use weather_db::StoredSnapshot;
use weather_schema::*;

use crate::runtime::Engine;

const DEFAULT_HISTORY_PAGE_SIZE: u32 = 7;
const MAX_HISTORY_PAGE_SIZE: u32 = 31;

#[derive(Clone, Copy, Default)]
struct TemperatureRange {
    max: Option<f64>,
    min: Option<f64>,
}

impl TemperatureRange {
    fn usable(self) -> bool {
        self.max.is_some() || self.min.is_some()
    }

    fn fill_missing(&mut self, other: Self) {
        if self.max.is_none() {
            self.max = other.max;
        }
        if self.min.is_none() {
            self.min = other.min;
        }
    }
}

impl Engine {
    pub(crate) async fn handle_get_temperature_history(&self, request: &RpcRequest) -> RpcResponse {
        let decoded = decode_message::<GetTemperatureHistoryRequest>(&request.payload);
        let Ok(req) = decoded else {
            return Self::rpc_error_response(
                &request.request_id,
                RpcErrorCode::BadRequest,
                decoded.unwrap_err().to_string(),
            );
        };
        let unified_uuid = req.unified_uuid.trim();
        if unified_uuid.is_empty() {
            return Self::rpc_error_response(
                &request.request_id,
                RpcErrorCode::BadRequest,
                "unified_uuid must not be empty",
            );
        }
        let page_size = if req.page_size == 0 {
            DEFAULT_HISTORY_PAGE_SIZE
        } else {
            req.page_size
        };
        if page_size > MAX_HISTORY_PAGE_SIZE {
            return Self::rpc_error_response(
                &request.request_id,
                RpcErrorCode::BadRequest,
                format!("page_size {page_size} exceeds maximum {MAX_HISTORY_PAGE_SIZE}"),
            );
        }
        let before_date = req.before_date.filter(|value| !value.trim().is_empty());
        if let Some(value) = before_date.as_deref()
            && (value.len() != 10 || NaiveDate::parse_from_str(value, "%Y-%m-%d").is_err())
        {
            return Self::rpc_error_response(
                &request.request_id,
                RpcErrorCode::BadRequest,
                format!("before_date `{value}` must use YYYY-MM-DD"),
            );
        }
        let include_forecast = before_date.is_none();
        match self
            .db
            .get_history_snapshot_page(unified_uuid.to_string(), before_date, page_size as usize)
            .await
        {
            Ok(page) => {
                let next_before_date = page
                    .has_more
                    .then(|| page.snapshots.first().map(|snapshot| snapshot.date.clone()))
                    .flatten();
                self.ok(
                    &request.request_id,
                    GetTemperatureHistoryResponse {
                        points: build_temperature_history(&page.snapshots, include_forecast),
                        next_before_date,
                        has_more_history: page.has_more,
                    },
                )
            }
            Err(error) => Self::rpc_error_response(
                &request.request_id,
                RpcErrorCode::Database,
                format!("failed to load temperature history: {error:#}"),
            ),
        }
    }
}

fn build_temperature_history(
    history: &[StoredSnapshot],
    include_forecast: bool,
) -> Vec<DailyTemperaturePoint> {
    let mut points = BTreeMap::<NaiveDate, DailyTemperaturePoint>::new();
    for stored in history {
        let Some(date) = parse_full_date(&stored.date) else {
            continue;
        };
        let range = historical_range(stored, date);
        if range.usable() {
            points.insert(date, daily_point(date, range, false));
        }
    }

    if include_forecast
        && let Some(latest) = history.last()
        && let Some(base_date) = parse_full_date(&latest.date)
    {
        if let Some(forecast) = latest.snapshot.predict.as_ref() {
            for day in &forecast.days {
                let Some(date) =
                    parse_relative_date(&day.date, base_date).filter(|date| *date >= base_date)
                else {
                    continue;
                };
                let range = TemperatureRange {
                    max: parse_temperature(day.day_temperature.as_deref()),
                    min: parse_temperature(day.night_temperature.as_deref()),
                };
                if !range.usable() {
                    continue;
                }
                points
                    .entry(date)
                    .and_modify(|point| {
                        if range.max.is_some() {
                            point.max_temperature = range.max;
                        }
                        if range.min.is_some() {
                            point.min_temperature = range.min;
                        }
                        point.forecast = true;
                    })
                    .or_insert_with(|| daily_point(date, range, true));
            }
        }
        for chart in &latest.snapshot.tempchart {
            let Some(date) = chart
                .date
                .as_deref()
                .and_then(|value| parse_relative_date(value, base_date))
                .filter(|date| *date >= base_date)
            else {
                continue;
            };
            let range = TemperatureRange {
                max: chart.max_temperature,
                min: chart.min_temperature,
            };
            if !range.usable() {
                continue;
            }
            points
                .entry(date)
                .and_modify(|point| {
                    if range.max.is_some() {
                        point.max_temperature = range.max;
                    }
                    if range.min.is_some() {
                        point.min_temperature = range.min;
                    }
                    point.forecast = true;
                })
                .or_insert_with(|| daily_point(date, range, true));
        }
    }

    points.into_values().collect()
}

fn historical_range(stored: &StoredSnapshot, date: NaiveDate) -> TemperatureRange {
    let matching_chart = stored
        .snapshot
        .tempchart
        .iter()
        .find(|chart| {
            chart
                .date
                .as_deref()
                .and_then(|value| parse_relative_date(value, date))
                == Some(date)
        })
        .or_else(|| {
            (stored.snapshot.tempchart.len() == 1
                && stored.snapshot.tempchart[0]
                    .date
                    .as_deref()
                    .is_none_or(|value| value.trim().is_empty()))
            .then(|| &stored.snapshot.tempchart[0])
        });
    let mut range =
        matching_chart.map_or_else(TemperatureRange::default, |chart| TemperatureRange {
            max: chart.max_temperature,
            min: chart.min_temperature,
        });

    let observed = stored
        .snapshot
        .passedchart
        .iter()
        .filter(|row| {
            row.time.as_deref().is_none_or(|time| {
                parse_relative_date(time, date).is_none_or(|row_date| row_date == date)
            })
        })
        .filter_map(|row| row.temperature)
        .collect::<Vec<_>>();
    if !observed.is_empty() {
        range.fill_missing(TemperatureRange {
            max: observed.iter().copied().reduce(f64::max),
            min: observed.iter().copied().reduce(f64::min),
        });
    }

    if let Some(day) = stored.snapshot.predict.as_ref().and_then(|forecast| {
        forecast
            .days
            .iter()
            .find(|day| parse_relative_date(&day.date, date) == Some(date))
    }) {
        range.fill_missing(TemperatureRange {
            max: parse_temperature(day.day_temperature.as_deref()),
            min: parse_temperature(day.night_temperature.as_deref()),
        });
    }
    range
}

fn daily_point(date: NaiveDate, range: TemperatureRange, forecast: bool) -> DailyTemperaturePoint {
    DailyTemperaturePoint {
        date: date.format("%Y-%m-%d").to_string(),
        max_temperature: range.max,
        min_temperature: range.min,
        forecast,
    }
}

pub(super) fn parse_temperature(value: Option<&str>) -> Option<f64> {
    value?
        .trim()
        .trim_end_matches(['°', '℃'])
        .trim()
        .parse()
        .ok()
}

pub(super) fn parse_full_date(value: &str) -> Option<NaiveDate> {
    let date = value.split_whitespace().next()?;
    ["%Y-%m-%d", "%Y/%m/%d", "%Y.%m.%d"]
        .into_iter()
        .find_map(|format| NaiveDate::parse_from_str(date, format).ok())
}

pub(super) fn parse_relative_date(value: &str, base: NaiveDate) -> Option<NaiveDate> {
    if let Some(date) = parse_full_date(value) {
        return Some(date);
    }
    let date = value.split_whitespace().next()?;
    let (month, day) = ["%m-%d", "%m/%d", "%m.%d"]
        .into_iter()
        .find_map(|format| {
            NaiveDate::parse_from_str(&format!("2000-{date}"), &format!("%Y-{format}")).ok()
        })
        .map(|date| (date.month(), date.day()))?;
    [base.year() - 1, base.year(), base.year() + 1]
        .into_iter()
        .filter_map(|year| NaiveDate::from_ymd_opt(year, month, day))
        .min_by_key(|candidate| (*candidate - base).num_days().unsigned_abs())
}

#[cfg(test)]
mod tests {
    use weather_db::StoredSnapshot;

    use super::*;

    fn stored(date: &str, snapshot: WeatherSnapshot) -> StoredSnapshot {
        StoredSnapshot {
            date: date.to_string(),
            snapshot,
            fetched_at_unix_ms: 0,
        }
    }

    #[test]
    fn combines_all_historical_dates_with_latest_forecast() {
        let older = WeatherSnapshot {
            tempchart: vec![TemperatureChart {
                date: Some("07-15".to_string()),
                max_temperature: Some(31.0),
                min_temperature: Some(22.0),
                ..Default::default()
            }],
            ..Default::default()
        };
        let latest = WeatherSnapshot {
            passedchart: vec![
                PassedWeatherChart {
                    time: Some("09:00".to_string()),
                    temperature: Some(24.0),
                    ..Default::default()
                },
                PassedWeatherChart {
                    time: Some("15:00".to_string()),
                    temperature: Some(30.0),
                    ..Default::default()
                },
            ],
            predict: Some(ForecastReport {
                publish_time: None,
                days: vec![
                    ForecastDay {
                        date: "07-16".to_string(),
                        day_temperature: Some("32℃".to_string()),
                        night_temperature: Some("23°".to_string()),
                        ..Default::default()
                    },
                    ForecastDay {
                        date: "07-17".to_string(),
                        day_temperature: Some("34".to_string()),
                        night_temperature: Some("25".to_string()),
                        ..Default::default()
                    },
                ],
            }),
            ..Default::default()
        };

        let points = build_temperature_history(
            &[stored("2026-07-15", older), stored("2026-07-16", latest)],
            true,
        );

        assert_eq!(points.len(), 3);
        assert_eq!(points[0].date, "2026-07-15");
        assert_eq!(points[0].max_temperature, Some(31.0));
        assert!(!points[0].forecast);
        assert_eq!(points[1].date, "2026-07-16");
        assert_eq!(points[1].max_temperature, Some(32.0));
        assert_eq!(points[1].min_temperature, Some(23.0));
        assert!(points[1].forecast);
        assert_eq!(points[2].date, "2026-07-17");
        assert!(points[2].forecast);
    }

    #[test]
    fn historical_rows_fall_back_to_observed_temperature_range() {
        let snapshot = WeatherSnapshot {
            passedchart: vec![
                PassedWeatherChart {
                    time: Some("2026-07-14 03:00".to_string()),
                    temperature: Some(19.5),
                    ..Default::default()
                },
                PassedWeatherChart {
                    time: Some("2026-07-14 14:00".to_string()),
                    temperature: Some(28.5),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let points = build_temperature_history(&[stored("2026-07-14", snapshot)], false);

        assert_eq!(points[0].max_temperature, Some(28.5));
        assert_eq!(points[0].min_temperature, Some(19.5));
    }

    #[test]
    fn older_history_pages_do_not_append_stale_forecasts() {
        let snapshot = WeatherSnapshot {
            predict: Some(ForecastReport {
                days: vec![ForecastDay {
                    date: "07-15".to_string(),
                    day_temperature: Some("30".to_string()),
                    night_temperature: Some("21".to_string()),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        };

        let points = build_temperature_history(&[stored("2026-07-15", snapshot)], false);

        assert_eq!(points.len(), 1);
        assert_eq!(points[0].date, "2026-07-15");
        assert!(!points[0].forecast);
    }

    #[test]
    fn month_day_dates_choose_the_year_nearest_to_the_latest_snapshot() {
        let base = NaiveDate::from_ymd_opt(2026, 12, 31).unwrap();
        assert_eq!(
            parse_relative_date("01-01", base),
            NaiveDate::from_ymd_opt(2027, 1, 1)
        );
    }
}
