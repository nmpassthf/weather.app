use std::time::Duration;

use weather_schema::GetWeatherRequest;

use crate::runtime::Engine;

const STAGGER: Duration = Duration::from_secs(5);

/// 启动后台任务,按配置 TTL 错开刷新所有启用的站点,刷新完成后通过 PUB 广播。
/// 返回顶层 task 的 JoinHandle,engine 退出时 abort 以避免泄漏。
pub(crate) fn spawn_refresh_loop(engine: Engine) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut config_rx = engine.config.subscribe();
        let mut stations = enabled_stations(&engine);
        let mut handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();
        spawn_refresh_tasks(&engine, &mut stations, &mut handles);
        while config_rx.changed().await.is_ok() {
            let new_stations = enabled_stations(&engine);
            if new_stations != stations {
                stations = new_stations;
                for handle in handles.drain(..) {
                    handle.abort();
                }
                spawn_refresh_tasks(&engine, &mut stations, &mut handles);
            }
        }
        for handle in handles {
            handle.abort();
        }
    })
}

fn enabled_stations(engine: &Engine) -> Vec<String> {
    engine
        .config
        .get()
        .stations
        .iter()
        .filter(|station| station.enabled)
        .map(|station| station.name.clone())
        .collect()
}

fn spawn_refresh_tasks(
    engine: &Engine,
    stations: &mut [String],
    handles: &mut Vec<tokio::task::JoinHandle<()>>,
) {
    for (index, name) in stations.iter().cloned().enumerate() {
        let engine = engine.clone();
        let initial_delay = STAGGER.saturating_mul(index as u32);
        handles.push(tokio::spawn(async move {
            tokio::time::sleep(initial_delay).await;
            loop {
                refresh_one(&engine, &name).await;
                let ttl = engine.config.get().updater.weather_ttl_seconds.max(1);
                tokio::time::sleep(Duration::from_secs(ttl)).await;
            }
        }));
    }
}

async fn refresh_one(engine: &Engine, name: &str) {
    let station = match engine.station_by_name(name).await {
        Ok(station) => station,
        Err(_) => return,
    };
    let unified_uuid = station.unified_uuid.clone();
    engine.publish_refresh(Some(&unified_uuid), true, false);
    let req = GetWeatherRequest {
        unified_uuid: unified_uuid.clone(),
        refresh: true,
        include_debug: false,
    };
    if let Ok(snapshot) = engine.get_weather_internal(req).await {
        engine.publish_refresh(Some(&unified_uuid), false, true);
        engine.publish_snapshot(&snapshot);
    }
}
