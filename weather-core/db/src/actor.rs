use std::path::PathBuf;

use anyhow::{Context, Result};
use tokio::sync::{mpsc, oneshot};
use weather_schema::{StationRef, WeatherSnapshot};

use crate::storage::DbInstance;

#[derive(Clone)]
pub struct DbActor {
    tx: mpsc::Sender<DbCommand>,
}

pub(crate) enum DbCommand {
    PutHistorySnapshot {
        snapshot: Box<WeatherSnapshot>,
        forecast_json: String,
        alerts_json: String,
        date: String,
        reply: oneshot::Sender<Result<()>>,
    },
    GetHistorySnapshot {
        uuid: String,
        date: String,
        reply: oneshot::Sender<Result<Option<StoredSnapshot>>>,
    },
    GetLatestSnapshot {
        uuid: String,
        reply: oneshot::Sender<Result<Option<StoredSnapshot>>>,
    },
    PutProviderProvinces {
        provinces: Vec<ProviderProvince>,
        reply: oneshot::Sender<Result<()>>,
    },
    GetProviderProvinces {
        reply: oneshot::Sender<Result<Vec<ProviderProvince>>>,
    },
    ResolveProviderProvinceCode {
        province: String,
        reply: oneshot::Sender<Result<String>>,
    },
    PutProviderCities {
        provider_province_code: String,
        cities: Vec<ProviderCity>,
        reply: oneshot::Sender<Result<()>>,
    },
    GetProviderCities {
        provider_province_code: String,
        reply: oneshot::Sender<Result<Vec<ProviderCity>>>,
    },
    GetProviderStationByUuid {
        provider: String,
        uuid: String,
        reply: oneshot::Sender<Result<Option<ProviderStation>>>,
    },
    PutProviderStationMapping {
        station: ProviderStation,
        reply: oneshot::Sender<Result<()>>,
    },
    GetProviderStationByName {
        provider: String,
        display_name: String,
        reply: oneshot::Sender<Result<Option<ProviderStation>>>,
    },
    GetDbTimezone {
        reply: oneshot::Sender<Result<Option<String>>>,
    },
    SetDbTimezone {
        timezone: String,
        reply: oneshot::Sender<Result<()>>,
    },
    MigrateTimezone {
        old_timezone: String,
        new_timezone: String,
        reply: oneshot::Sender<Result<u64>>,
    },
    LogFetch {
        unified_uuid: Option<String>,
        endpoint: String,
        ok: bool,
        message: Option<String>,
        reply: oneshot::Sender<Result<()>>,
    },
    /// graceful shutdown:checkpoint WAL 后退出 db 线程。
    Shutdown { reply: oneshot::Sender<Result<()>> },
}

#[derive(Debug)]
pub struct StoredSnapshot {
    pub snapshot: WeatherSnapshot,
    pub fetched_at_unix_ms: i64,
}

#[derive(Debug, Clone)]
pub struct ProviderProvince {
    pub provider_code: String,
    pub name: String,
    pub url: String,
}

impl ProviderProvince {
    pub fn public_ref(&self) -> weather_schema::Province {
        weather_schema::Province {
            name: self.name.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProviderCity {
    pub provider_code: String,
    pub provider_province_code: String,
    pub province: String,
    pub city: String,
    pub url: String,
}

impl ProviderCity {
    pub fn public_ref(&self) -> weather_schema::City {
        weather_schema::City {
            province: self.province.clone(),
            city: self.city.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProviderStation {
    pub provider_name: String,
    pub display_name: String,
    pub provider_station_id: String,
    pub provider_province_code: String,
    pub province: String,
    pub city: String,
    pub url: String,
    pub name: String,
    pub unified_uuid: String,
}

impl ProviderStation {
    pub fn public_ref(&self) -> StationRef {
        StationRef {
            province: self.province.clone(),
            city: self.city.clone(),
            name: self.name.clone(),
            unified_uuid: self.unified_uuid.clone(),
        }
    }
}

impl DbActor {
    pub fn start(path: PathBuf, config_tz: String) -> Result<Self> {
        let (tx, mut rx) = mpsc::channel::<DbCommand>(128);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = DbInstance::open(path, &config_tz);
            match result {
                Ok(db) => {
                    let _ = ready_tx.send(Ok(()));
                    while let Some(cmd) = rx.blocking_recv() {
                        if matches!(cmd, DbCommand::Shutdown { .. }) {
                            handle(&db, cmd);
                            break;
                        }
                        handle(&db, cmd);
                    }
                }
                Err(err) => {
                    let _ = ready_tx.send(Err(err));
                }
            }
        });
        ready_rx.recv().context("DB actor failed to start")??;
        Ok(Self { tx })
    }

    pub async fn put_history_snapshot(
        &self,
        snapshot: WeatherSnapshot,
        forecast_json: String,
        alerts_json: String,
        date: String,
    ) -> Result<()> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::PutHistorySnapshot {
                snapshot: Box::new(snapshot),
                forecast_json,
                alerts_json,
                date,
                reply,
            })
            .await?;
        rx.await?
    }

    pub async fn get_history_snapshot(
        &self,
        uuid: String,
        date: String,
    ) -> Result<Option<StoredSnapshot>> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::GetHistorySnapshot { uuid, date, reply })
            .await?;
        rx.await?
    }

    pub async fn get_latest_snapshot(&self, uuid: String) -> Result<Option<StoredSnapshot>> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::GetLatestSnapshot { uuid, reply })
            .await?;
        rx.await?
    }

    pub async fn put_provider_provinces(&self, provinces: Vec<ProviderProvince>) -> Result<()> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::PutProviderProvinces { provinces, reply })
            .await?;
        rx.await?
    }

    pub async fn get_provider_provinces(&self) -> Result<Vec<ProviderProvince>> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::GetProviderProvinces { reply })
            .await?;
        rx.await?
    }

    pub async fn resolve_provider_province_code(&self, province: String) -> Result<String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::ResolveProviderProvinceCode { province, reply })
            .await?;
        rx.await?
    }

    pub async fn put_provider_cities(
        &self,
        provider_province_code: String,
        cities: Vec<ProviderCity>,
    ) -> Result<()> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::PutProviderCities {
                provider_province_code,
                cities,
                reply,
            })
            .await?;
        rx.await?
    }

    pub async fn get_provider_cities(
        &self,
        provider_province_code: String,
    ) -> Result<Vec<ProviderCity>> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::GetProviderCities {
                provider_province_code,
                reply,
            })
            .await?;
        rx.await?
    }

    pub async fn get_provider_station_by_uuid(
        &self,
        provider: String,
        uuid: String,
    ) -> Result<Option<ProviderStation>> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::GetProviderStationByUuid {
                provider,
                uuid,
                reply,
            })
            .await?;
        rx.await?
    }

    pub async fn put_provider_station_mapping(&self, station: ProviderStation) -> Result<()> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::PutProviderStationMapping { station, reply })
            .await?;
        rx.await?
    }

    pub async fn get_provider_station_by_name(
        &self,
        provider: String,
        display_name: String,
    ) -> Result<Option<ProviderStation>> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::GetProviderStationByName {
                provider,
                display_name,
                reply,
            })
            .await?;
        rx.await?
    }

    pub async fn get_db_timezone(&self) -> Result<Option<String>> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(DbCommand::GetDbTimezone { reply }).await?;
        rx.await?
    }

    pub async fn set_db_timezone(&self, timezone: String) -> Result<()> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::SetDbTimezone { timezone, reply })
            .await?;
        rx.await?
    }

    pub async fn migrate_timezone(
        &self,
        old_timezone: String,
        new_timezone: String,
    ) -> Result<u64> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::MigrateTimezone {
                old_timezone,
                new_timezone,
                reply,
            })
            .await?;
        rx.await?
    }

    pub async fn log_fetch(
        &self,
        unified_uuid: Option<String>,
        endpoint: String,
        ok: bool,
        message: Option<String>,
    ) -> Result<()> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::LogFetch {
                unified_uuid,
                endpoint,
                ok,
                message,
                reply,
            })
            .await?;
        rx.await?
    }

    /// 触发 db actor graceful shutdown:checkpoint WAL 后退出线程。
    pub async fn shutdown(&self) -> Result<()> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(DbCommand::Shutdown { reply }).await?;
        rx.await?
    }
}

fn handle(db: &DbInstance, cmd: DbCommand) {
    match cmd {
        DbCommand::PutHistorySnapshot {
            snapshot,
            forecast_json,
            alerts_json,
            date,
            reply,
        } => {
            let _ =
                reply.send(db.put_history_snapshot(&snapshot, &forecast_json, &alerts_json, &date));
        }
        DbCommand::GetHistorySnapshot { uuid, date, reply } => {
            let _ = reply.send(db.get_history_snapshot(&uuid, &date));
        }
        DbCommand::GetLatestSnapshot { uuid, reply } => {
            let _ = reply.send(db.get_latest_snapshot(&uuid));
        }
        DbCommand::PutProviderProvinces { provinces, reply } => {
            let _ = reply.send(db.put_provider_provinces(&provinces));
        }
        DbCommand::GetProviderProvinces { reply } => {
            let _ = reply.send(db.get_provider_provinces());
        }
        DbCommand::ResolveProviderProvinceCode { province, reply } => {
            let _ = reply.send(db.resolve_provider_province_code(&province));
        }
        DbCommand::PutProviderCities {
            provider_province_code,
            cities,
            reply,
        } => {
            let _ = reply.send(db.put_provider_cities(&provider_province_code, &cities));
        }
        DbCommand::GetProviderCities {
            provider_province_code,
            reply,
        } => {
            let _ = reply.send(db.get_provider_cities(&provider_province_code));
        }
        DbCommand::GetProviderStationByUuid {
            provider,
            uuid,
            reply,
        } => {
            let _ = reply.send(db.get_provider_station_by_uuid(&provider, &uuid));
        }
        DbCommand::PutProviderStationMapping { station, reply } => {
            let _ = reply.send(db.put_provider_station_mapping(&station));
        }
        DbCommand::GetProviderStationByName {
            provider,
            display_name,
            reply,
        } => {
            let _ = reply.send(db.get_provider_station_by_name(&provider, &display_name));
        }
        DbCommand::GetDbTimezone { reply } => {
            let _ = reply.send(db.get_db_timezone());
        }
        DbCommand::SetDbTimezone { timezone, reply } => {
            let _ = reply.send(db.set_db_timezone(&timezone));
        }
        DbCommand::MigrateTimezone {
            old_timezone,
            new_timezone,
            reply,
        } => {
            let _ = reply.send(db.migrate_timezone(&old_timezone, &new_timezone));
        }
        DbCommand::LogFetch {
            unified_uuid,
            endpoint,
            ok,
            message,
            reply,
        } => {
            let _ = reply.send(db.log_fetch(
                unified_uuid.as_deref(),
                &endpoint,
                ok,
                message.as_deref(),
            ));
        }
        DbCommand::Shutdown { reply } => {
            let _ = reply.send(db.checkpoint());
        }
    }
}
