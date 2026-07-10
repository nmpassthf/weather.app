use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use tokio::task::{Id, JoinSet};
use weather_configure::AppConfig;
use weather_schema::{GetWeatherRequest, unix_timestamp_ms};

use crate::{
    handlers::RefreshTerminal, lifecycle::Cancellation, limits::MAX_CONCURRENT_REFRESHES,
    runtime::Engine, time::local_date_and_next_change,
};

const STAGGER: Duration = Duration::from_secs(5);
const FALLBACK_WAKE: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
struct StationSchedule {
    last_completed: Option<Instant>,
    next_due: Instant,
    in_flight: bool,
    date_refresh_pending: bool,
}

/// Runs one wakeable scheduler for every configured station. Fetch jobs are
/// short-lived members of the scheduler's JoinSet and are always drained on
/// cancellation.
pub(crate) async fn run_refresh_loop(
    engine: Engine,
    cancellation: Cancellation,
) -> anyhow::Result<()> {
    let mut config_rx = engine.config.subscribe();
    let initial = engine.config.get();
    let mut station_order = enabled_stations(&initial);
    let mut ttl = weather_ttl(&initial);
    let now = Instant::now();
    let mut schedules = HashMap::new();
    reconcile_schedules(&mut schedules, &station_order, now, ttl);

    let mut last_local_date = local_date_and_next_change(
        unix_timestamp_ms().unwrap_or_default(),
        &initial.db.timezone,
    )
    .ok()
    .map(|(date, _)| date);
    let mut jobs = JoinSet::new();
    let mut job_names = HashMap::<Id, String>::new();

    let result = loop {
        let config = engine.config.get();
        let now = Instant::now();
        let (current_date, date_wait) = match local_date_and_next_change(
            unix_timestamp_ms().unwrap_or_default(),
            &config.db.timezone,
        ) {
            Ok((date, wait)) => (Some(date), wait),
            Err(_) => (None, FALLBACK_WAKE),
        };
        if let Some(current_date) = current_date {
            if last_local_date
                .as_ref()
                .is_some_and(|previous| previous != &current_date)
            {
                schedule_date_refresh(&mut schedules, &station_order, now);
            }
            last_local_date = Some(current_date);
        }

        dispatch_due(
            &engine,
            &station_order,
            &mut schedules,
            &mut jobs,
            &mut job_names,
            now,
            ttl,
        );

        let scheduler_wait = next_scheduler_wait(&schedules, now, jobs.len());
        let wait = scheduler_wait.min(date_wait).min(FALLBACK_WAKE);
        let has_jobs = !jobs.is_empty();
        tokio::select! {
            _ = cancellation.cancelled() => break Ok(()),
            changed = config_rx.changed() => {
                if changed.is_err() {
                    break Err(anyhow::anyhow!("refresh config watch closed unexpectedly"));
                }
                let config = config_rx.borrow_and_update().clone();
                station_order = enabled_stations(&config);
                ttl = weather_ttl(&config);
                reconcile_schedules(
                    &mut schedules,
                    &station_order,
                    Instant::now(),
                    ttl,
                );
            }
            joined = jobs.join_next_with_id(), if has_jobs => {
                if let Some(joined) = joined {
                    let (id, failed) = match joined {
                        Ok((id, ())) => (id, None),
                        Err(err) => {
                            let id = err.id();
                            (id, Some(err.to_string()))
                        }
                    };
                    if let Some(name) = job_names.remove(&id) {
                        finish_job(&mut schedules, &name, Instant::now(), ttl);
                        if let Some(message) = failed {
                            let uuid = weather_schema::unified_station_uuid(&name);
                            engine.publish_refresh_terminal(
                                Some(&uuid),
                                RefreshTerminal::Failure(format!("refresh task failed: {message}")),
                            );
                        }
                    }
                }
            }
            _ = tokio::time::sleep(wait) => {}
        }
    };

    jobs.abort_all();
    while jobs.join_next().await.is_some() {}
    result
}

fn enabled_stations(config: &AppConfig) -> Vec<String> {
    let mut seen = HashSet::new();
    config
        .stations
        .iter()
        .filter(|station| station.enabled)
        .filter(|station| seen.insert(station.name.clone()))
        .map(|station| station.name.clone())
        .collect()
}

fn weather_ttl(config: &AppConfig) -> Duration {
    Duration::from_secs(config.updater.weather_ttl_seconds.max(1))
}

fn reconcile_schedules(
    schedules: &mut HashMap<String, StationSchedule>,
    station_order: &[String],
    now: Instant,
    ttl: Duration,
) {
    let enabled = station_order.iter().collect::<HashSet<_>>();
    schedules.retain(|name, _| enabled.contains(name));

    for (index, name) in station_order.iter().enumerate() {
        if let Some(schedule) = schedules.get_mut(name) {
            if !schedule.date_refresh_pending
                && let Some(last_completed) = schedule.last_completed
            {
                schedule.next_due = deadline(last_completed, ttl);
            }
        } else {
            schedules.insert(
                name.clone(),
                StationSchedule {
                    last_completed: None,
                    next_due: deadline(now, stagger(index)),
                    in_flight: false,
                    date_refresh_pending: false,
                },
            );
        }
    }
}

fn schedule_date_refresh(
    schedules: &mut HashMap<String, StationSchedule>,
    station_order: &[String],
    now: Instant,
) {
    for (index, name) in station_order.iter().enumerate() {
        if let Some(schedule) = schedules.get_mut(name) {
            schedule.date_refresh_pending = true;
            if !schedule.in_flight {
                schedule.next_due = deadline(now, stagger(index));
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn dispatch_due(
    engine: &Engine,
    station_order: &[String],
    schedules: &mut HashMap<String, StationSchedule>,
    jobs: &mut JoinSet<()>,
    job_names: &mut HashMap<Id, String>,
    now: Instant,
    ttl: Duration,
) {
    let available = MAX_CONCURRENT_REFRESHES.saturating_sub(jobs.len());
    let claimed = claim_due_stations(station_order, schedules, now, ttl, available);
    for name in claimed {
        let engine = engine.clone();
        let job_name = name.clone();
        let handle = jobs.spawn(async move {
            refresh_one(&engine, &job_name).await;
        });
        job_names.insert(handle.id(), name);
    }
}

fn claim_due_stations(
    station_order: &[String],
    schedules: &mut HashMap<String, StationSchedule>,
    now: Instant,
    ttl: Duration,
    available: usize,
) -> Vec<String> {
    let mut claimed = Vec::with_capacity(available.min(station_order.len()));
    for name in station_order {
        if claimed.len() >= available {
            break;
        }
        let Some(schedule) = schedules.get_mut(name) else {
            continue;
        };
        if schedule.in_flight || schedule.next_due > now {
            continue;
        }

        schedule.in_flight = true;
        schedule.date_refresh_pending = false;
        schedule.next_due = deadline(now, ttl);
        claimed.push(name.clone());
    }
    claimed
}

fn finish_job(
    schedules: &mut HashMap<String, StationSchedule>,
    name: &str,
    completed_at: Instant,
    ttl: Duration,
) {
    if let Some(schedule) = schedules.get_mut(name) {
        schedule.in_flight = false;
        schedule.last_completed = Some(completed_at);
        schedule.next_due = if schedule.date_refresh_pending {
            completed_at
        } else {
            deadline(completed_at, ttl)
        };
    }
}

fn next_scheduler_wait(
    schedules: &HashMap<String, StationSchedule>,
    now: Instant,
    active_jobs: usize,
) -> Duration {
    if active_jobs >= MAX_CONCURRENT_REFRESHES {
        return FALLBACK_WAKE;
    }
    schedules
        .values()
        .filter(|schedule| !schedule.in_flight)
        .map(|schedule| schedule.next_due.saturating_duration_since(now))
        .min()
        .unwrap_or(FALLBACK_WAKE)
}

fn deadline(start: Instant, delay: Duration) -> Instant {
    start.checked_add(delay).unwrap_or(start)
}

fn stagger(index: usize) -> Duration {
    STAGGER.saturating_mul(u32::try_from(index).unwrap_or(u32::MAX))
}

async fn refresh_one(engine: &Engine, name: &str) {
    let req = GetWeatherRequest {
        unified_uuid: weather_schema::unified_station_uuid(name),
        refresh: true,
        include_debug: false,
    };
    if let Ok(snapshot) = engine.get_weather_internal(req).await {
        engine.publish_snapshot(&snapshot);
    }
}

#[cfg(test)]
mod tests {
    use weather_configure::StationConfig;

    use super::*;

    #[test]
    fn enabled_station_names_are_deduplicated_in_order() {
        let config = AppConfig {
            stations: vec![
                StationConfig {
                    name: "A".to_string(),
                    enabled: true,
                },
                StationConfig {
                    name: "A".to_string(),
                    enabled: true,
                },
                StationConfig {
                    name: "B".to_string(),
                    enabled: false,
                },
                StationConfig {
                    name: "C".to_string(),
                    enabled: true,
                },
            ],
            ..Default::default()
        };

        assert_eq!(enabled_stations(&config), vec!["A", "C"]);
    }

    #[test]
    fn ttl_change_recomputes_deadline_from_last_completion() {
        let started = Instant::now();
        let mut schedules = HashMap::new();
        reconcile_schedules(
            &mut schedules,
            &["A".to_string()],
            started,
            Duration::from_secs(60),
        );
        finish_job(&mut schedules, "A", started, Duration::from_secs(60));

        reconcile_schedules(
            &mut schedules,
            &["A".to_string()],
            deadline(started, Duration::from_secs(10)),
            Duration::from_secs(5),
        );

        assert_eq!(
            schedules["A"].next_due,
            deadline(started, Duration::from_secs(5))
        );
    }

    #[test]
    fn local_date_change_schedules_all_stations_with_stagger() {
        let now = Instant::now();
        let names = vec!["A".to_string(), "B".to_string()];
        let mut schedules = HashMap::new();
        reconcile_schedules(&mut schedules, &names, now, Duration::from_secs(60));
        finish_job(&mut schedules, "A", now, Duration::from_secs(60));
        finish_job(&mut schedules, "B", now, Duration::from_secs(60));

        schedule_date_refresh(&mut schedules, &names, now);

        assert_eq!(schedules["A"].next_due, now);
        assert_eq!(schedules["B"].next_due, deadline(now, STAGGER));
        assert!(schedules["A"].date_refresh_pending);
        assert!(schedules["B"].date_refresh_pending);
    }

    #[test]
    fn pending_date_refresh_survives_ttl_reconciliation() {
        let now = Instant::now();
        let names = vec!["A".to_string()];
        let mut schedules = HashMap::new();
        reconcile_schedules(&mut schedules, &names, now, Duration::from_secs(60));
        finish_job(&mut schedules, "A", now, Duration::from_secs(60));
        let date_due = deadline(now, Duration::from_secs(10));
        schedules.get_mut("A").unwrap().next_due = date_due;
        schedules.get_mut("A").unwrap().date_refresh_pending = true;

        reconcile_schedules(
            &mut schedules,
            &names,
            deadline(now, Duration::from_secs(1)),
            Duration::from_secs(600),
        );

        assert_eq!(schedules["A"].next_due, date_due);
    }

    #[test]
    fn local_date_change_during_flight_is_refreshed_after_completion() {
        let now = Instant::now();
        let names = vec!["A".to_string()];
        let mut schedules = HashMap::new();
        reconcile_schedules(&mut schedules, &names, now, Duration::from_secs(60));
        schedules.get_mut("A").unwrap().in_flight = true;

        schedule_date_refresh(&mut schedules, &names, deadline(now, STAGGER));
        assert!(schedules["A"].date_refresh_pending);

        let completed = deadline(now, Duration::from_secs(10));
        finish_job(&mut schedules, "A", completed, Duration::from_secs(60));

        assert!(!schedules["A"].in_flight);
        assert_eq!(schedules["A"].next_due, completed);
        assert!(schedules["A"].date_refresh_pending);
    }

    #[test]
    fn due_refreshes_are_claimed_up_to_concurrency_limit() {
        let now = Instant::now();
        let names = (0..(MAX_CONCURRENT_REFRESHES + 3))
            .map(|index| format!("station-{index}"))
            .collect::<Vec<_>>();
        let mut schedules = HashMap::new();
        reconcile_schedules(&mut schedules, &names, now, Duration::from_secs(60));
        for schedule in schedules.values_mut() {
            schedule.next_due = now;
        }

        let claimed = claim_due_stations(
            &names,
            &mut schedules,
            now,
            Duration::from_secs(60),
            MAX_CONCURRENT_REFRESHES,
        );

        assert_eq!(claimed.len(), MAX_CONCURRENT_REFRESHES);
        assert!(claimed.iter().all(|name| schedules[name].in_flight));
        for name in names.iter().skip(MAX_CONCURRENT_REFRESHES) {
            assert!(!schedules[name].in_flight);
            assert_eq!(schedules[name].next_due, now);
        }
        assert_eq!(
            next_scheduler_wait(&schedules, now, MAX_CONCURRENT_REFRESHES),
            FALLBACK_WAKE
        );
        assert_eq!(
            next_scheduler_wait(&schedules, now, MAX_CONCURRENT_REFRESHES - 1),
            Duration::ZERO
        );
    }
}
