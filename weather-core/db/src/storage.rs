use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use weather_schema::{StationRef, WeatherSnapshot, unified_station_uuid};

use crate::actor::{ProviderCity, ProviderProvince, ProviderStation, StoredSnapshot};

pub(crate) struct DbInstance {
    conn: Connection,
}

impl DbInstance {
    pub(crate) fn open(path: PathBuf, config_tz: &str) -> Result<Self> {
        let conn = Connection::open(path).context("failed to open sqlite database")?;
        let db = Self { conn };
        db.migrate()?;
        db.ensure_timezone(config_tz)?;
        Ok(db)
    }

    fn migrate(&self) -> Result<()> {
        self.conn
            .execute_batch(
                r#"
        PRAGMA journal_mode = WAL;
        PRAGMA busy_timeout = 5000;
        CREATE TABLE IF NOT EXISTS schema_migrations(version INTEGER PRIMARY KEY);
        CREATE TABLE IF NOT EXISTS provider_provinces(provider_code TEXT PRIMARY KEY, name TEXT NOT NULL, url TEXT NOT NULL, updated_at_unix_ms INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS provider_cities(provider_code TEXT PRIMARY KEY, provider_province_code TEXT NOT NULL, province TEXT NOT NULL, city TEXT NOT NULL, url TEXT NOT NULL, updated_at_unix_ms INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS public_stations(unified_uuid TEXT PRIMARY KEY, province TEXT, city TEXT, name TEXT NOT NULL DEFAULT '', updated_at_unix_ms INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS provider_station_mappings(provider TEXT NOT NULL, display_name TEXT NOT NULL, provider_station_id TEXT NOT NULL, provider_province_code TEXT NOT NULL, province TEXT NOT NULL, city TEXT NOT NULL, url TEXT NOT NULL, name TEXT NOT NULL, unified_uuid TEXT NOT NULL, updated_at_unix_ms INTEGER NOT NULL, PRIMARY KEY(provider, display_name));
        CREATE INDEX IF NOT EXISTS idx_provider_station_mappings_uuid ON provider_station_mappings(provider, unified_uuid);
        CREATE TABLE IF NOT EXISTS alerts(id INTEGER PRIMARY KEY AUTOINCREMENT, unified_uuid TEXT NOT NULL, alert_json TEXT NOT NULL, fetched_at_unix_ms INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS upstream_fetch_log(id INTEGER PRIMARY KEY AUTOINCREMENT, unified_uuid TEXT, endpoint TEXT NOT NULL, ok INTEGER NOT NULL, message TEXT, fetched_at_unix_ms INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS engine_state(key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at_unix_ms INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS weather_snapshots_history(unified_uuid TEXT NOT NULL, date TEXT NOT NULL, station_name TEXT NOT NULL, snapshot_json TEXT NOT NULL, forecast_json TEXT NOT NULL, alerts_json TEXT NOT NULL, fetched_at_unix_ms INTEGER NOT NULL, PRIMARY KEY(unified_uuid, date));
        CREATE INDEX IF NOT EXISTS idx_history_uuid_fetched ON weather_snapshots_history(unified_uuid, fetched_at_unix_ms DESC);
        DROP TABLE IF EXISTS weather_snapshots;
        DROP TABLE IF EXISTS forecast_days;
        "#,
            )
            .context("failed to migrate sqlite schema")?;
        Ok(())
    }

    /// 校验 DB 中记录的时区与 config 一致；首次启动写入 config 时区。
    fn ensure_timezone(&self, config_tz: &str) -> Result<()> {
        let stored = self.get_db_timezone()?;
        match stored {
            None => {
                self.set_db_timezone(config_tz)?;
                Ok(())
            }
            Some(stored) if stored == config_tz => Ok(()),
            Some(stored) => bail!(
                "DB timezone `{stored}` != config `{config_tz}`; call MIGRATE_DB_TIMEZONE RPC to migrate"
            ),
        }
    }

    pub(crate) fn put_history_snapshot(
        &self,
        snapshot: &WeatherSnapshot,
        forecast_json: &str,
        alerts_json: &str,
        date: &str,
    ) -> Result<()> {
        let station = snapshot.station.as_ref();
        let uuid = station.map(|s| s.unified_uuid.clone()).unwrap_or_default();
        let station_name = station.map(|s| s.name.clone()).unwrap_or_default();
        let snapshot_json = serde_json::to_string(snapshot)?;
        let now = now_ms();
        self.conn.execute(
            r#"INSERT INTO weather_snapshots_history(unified_uuid, date, station_name, snapshot_json, forecast_json, alerts_json, fetched_at_unix_ms)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
               ON CONFLICT(unified_uuid, date) DO UPDATE SET
                 station_name = excluded.station_name,
                 snapshot_json = excluded.snapshot_json,
                 forecast_json = excluded.forecast_json,
                 alerts_json = excluded.alerts_json,
                 fetched_at_unix_ms = excluded.fetched_at_unix_ms"#,
            params![uuid, date, station_name, snapshot_json, forecast_json, alerts_json, now],
        )?;
        if let Some(station) = station {
            self.put_public_station(station)?;
        }
        Ok(())
    }

    pub(crate) fn get_history_snapshot(
        &self,
        uuid: &str,
        date: &str,
    ) -> Result<Option<StoredSnapshot>> {
        self.conn
            .query_row(
                "SELECT snapshot_json, fetched_at_unix_ms FROM weather_snapshots_history WHERE unified_uuid = ?1 AND date = ?2",
                params![uuid, date],
                |row| {
                    let json: String = row.get(0)?;
                    let fetched_at_unix_ms: i64 = row.get(1)?;
                    Ok((json, fetched_at_unix_ms))
                },
            )
            .optional()?
            .map(|(json, fetched_at_unix_ms)| {
                let snapshot = serde_json::from_str(&json)?;
                Ok(StoredSnapshot {
                    snapshot,
                    fetched_at_unix_ms,
                })
            })
            .transpose()
    }

    pub(crate) fn get_latest_snapshot(&self, uuid: &str) -> Result<Option<StoredSnapshot>> {
        let mut stmt = self.conn.prepare(
            "SELECT snapshot_json, fetched_at_unix_ms FROM weather_snapshots_history WHERE unified_uuid = ?1 ORDER BY fetched_at_unix_ms DESC",
        )?;
        let rows = stmt.query_map(params![uuid], |row| {
            let json: String = row.get(0)?;
            let fetched_at_unix_ms: i64 = row.get(1)?;
            Ok((json, fetched_at_unix_ms))
        })?;
        for row in rows {
            let (json, fetched_at_unix_ms) = row?;
            if let Ok(snapshot) = serde_json::from_str(&json) {
                return Ok(Some(StoredSnapshot {
                    snapshot,
                    fetched_at_unix_ms,
                }));
            }
        }
        Ok(None)
    }

    pub(crate) fn put_provider_provinces(&self, provinces: &[ProviderProvince]) -> Result<()> {
        let now = now_ms();
        for province in provinces {
            self.conn.execute(
                "INSERT OR REPLACE INTO provider_provinces(provider_code, name, url, updated_at_unix_ms) VALUES (?1, ?2, ?3, ?4)",
                params![province.provider_code, province.name, province.url, now],
            )?;
        }
        Ok(())
    }

    pub(crate) fn get_provider_provinces(&self) -> Result<Vec<ProviderProvince>> {
        let mut stmt = self.conn.prepare(
            "SELECT provider_code, name, url FROM provider_provinces ORDER BY provider_code",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ProviderProvince {
                provider_code: row.get(0)?,
                name: row.get(1)?,
                url: row.get(2)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub(crate) fn resolve_provider_province_code(&self, province: &str) -> Result<String> {
        let mut stmt = self.conn.prepare(
            "SELECT provider_code FROM provider_provinces WHERE name = ?1 ORDER BY provider_code",
        )?;
        let rows = stmt
            .query_map(params![province], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        match rows.as_slice() {
            [code] => Ok(code.clone()),
            [] => bail!("provider province `{province}` not found"),
            codes => bail!(
                "provider province `{province}` is ambiguous: {}",
                codes.join(", ")
            ),
        }
    }

    pub(crate) fn put_provider_cities(
        &self,
        provider_province_code: &str,
        cities: &[ProviderCity],
    ) -> Result<()> {
        let now = now_ms();
        for city in cities {
            self.conn.execute(
                "INSERT OR REPLACE INTO provider_cities(provider_code, provider_province_code, province, city, url, updated_at_unix_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![city.provider_code, provider_province_code, city.province, city.city, city.url, now],
            )?;
        }
        Ok(())
    }

    pub(crate) fn get_provider_cities(
        &self,
        provider_province_code: &str,
    ) -> Result<Vec<ProviderCity>> {
        let mut stmt = self.conn.prepare(
            "SELECT provider_code, province, city, url FROM provider_cities WHERE provider_province_code = ?1 ORDER BY city",
        )?;
        let rows = stmt.query_map(params![provider_province_code], |row| {
            Ok(ProviderCity {
                provider_code: row.get(0)?,
                provider_province_code: provider_province_code.to_string(),
                province: row.get(1)?,
                city: row.get(2)?,
                url: row.get(3)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub(crate) fn put_public_station(&self, station: &StationRef) -> Result<()> {
        let uuid = if station.unified_uuid.is_empty() {
            unified_station_uuid(&station.name)
        } else {
            station.unified_uuid.clone()
        };
        self.conn.execute(
            "INSERT OR REPLACE INTO public_stations(unified_uuid, province, city, name, updated_at_unix_ms) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![uuid, station.province, station.city, station.name, now_ms()],
        )?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn get_public_station_by_uuid(&self, uuid: &str) -> Result<Option<StationRef>> {
        self.conn
            .query_row(
                "SELECT province, city, name, unified_uuid FROM public_stations WHERE unified_uuid = ?1",
                params![uuid],
                map_public_station_ref,
            )
            .optional()
            .map_err(Into::into)
    }

    pub(crate) fn put_provider_station_mapping(&self, station: &ProviderStation) -> Result<()> {
        self.put_public_station(&station.public_ref())?;
        self.conn.execute(
            "INSERT OR REPLACE INTO provider_station_mappings(provider, display_name, provider_station_id, provider_province_code, province, city, url, name, unified_uuid, updated_at_unix_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                station.provider_name,
                station.display_name,
                station.provider_station_id,
                station.provider_province_code,
                station.province,
                station.city,
                station.url,
                station.name,
                station.unified_uuid,
                now_ms()
            ],
        )?;
        Ok(())
    }

    pub(crate) fn get_provider_station_by_name(
        &self,
        provider: &str,
        display_name: &str,
    ) -> Result<Option<ProviderStation>> {
        self.conn
            .query_row(
                "SELECT provider, display_name, provider_station_id, provider_province_code, province, city, url, name, unified_uuid FROM provider_station_mappings WHERE provider = ?1 AND display_name = ?2",
                params![provider, display_name],
                map_provider_station,
            )
            .optional()
            .map_err(Into::into)
    }

    pub(crate) fn get_provider_station_by_uuid(
        &self,
        provider: &str,
        uuid: &str,
    ) -> Result<Option<ProviderStation>> {
        self.conn
            .query_row(
                "SELECT provider, display_name, provider_station_id, provider_province_code, province, city, url, name, unified_uuid FROM provider_station_mappings WHERE provider = ?1 AND unified_uuid = ?2 ORDER BY display_name LIMIT 1",
                params![provider, uuid],
                map_provider_station,
            )
            .optional()
            .map_err(Into::into)
    }

    pub(crate) fn get_db_timezone(&self) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT value FROM engine_state WHERE key = 'db_timezone'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub(crate) fn set_db_timezone(&self, tz: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO engine_state(key, value, updated_at_unix_ms) VALUES ('db_timezone', ?1, ?2)",
            params![tz, now_ms()],
        )?;
        Ok(())
    }

    /// 把历史表所有行的 date 列按 `new_timezone` 重算。
    ///
    /// PK 冲突（同 uuid 同新 date 已存在）时保留 `fetched_at_unix_ms` 较大者。
    /// 整个迁移在单事务内完成，避免半迁移状态。返回受影响行数。
    pub(crate) fn migrate_timezone(&self, old_tz: &str, new_tz: &str) -> Result<u64> {
        if old_tz == new_tz {
            return Ok(0);
        }
        let new_tz_obj = chrono_tz::Tz::from_str(new_tz)
            .map_err(|_| anyhow::anyhow!("invalid new timezone `{new_tz}`"))?;
        let mut rows = self.conn.prepare(
            "SELECT unified_uuid, date, snapshot_json, forecast_json, alerts_json, fetched_at_unix_ms FROM weather_snapshots_history",
        )?;
        let entries: Vec<(String, String, String, String, String, i64)> = rows
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(rows);

        let tx = self.conn.unchecked_transaction()?;
        let mut rewritten = 0u64;
        for (uuid, old_date, snapshot_json, forecast_json, alerts_json, fetched_at) in entries {
            let dt = DateTime::<Utc>::from_timestamp_millis(fetched_at)
                .ok_or_else(|| anyhow::anyhow!("invalid fetched_at_unix_ms {fetched_at}"))?;
            let new_date = dt.with_timezone(&new_tz_obj).format("%Y-%m-%d").to_string();
            if new_date == old_date {
                continue;
            }
            tx.execute(
                r#"INSERT INTO weather_snapshots_history(unified_uuid, date, station_name, snapshot_json, forecast_json, alerts_json, fetched_at_unix_ms)
                   VALUES (?1, ?2, '', ?3, ?4, ?5, ?6)
                   ON CONFLICT(unified_uuid, date) DO UPDATE SET
                     snapshot_json = CASE WHEN excluded.fetched_at_unix_ms > weather_snapshots_history.fetched_at_unix_ms THEN excluded.snapshot_json ELSE weather_snapshots_history.snapshot_json END,
                     forecast_json = CASE WHEN excluded.fetched_at_unix_ms > weather_snapshots_history.fetched_at_unix_ms THEN excluded.forecast_json ELSE weather_snapshots_history.forecast_json END,
                     alerts_json = CASE WHEN excluded.fetched_at_unix_ms > weather_snapshots_history.fetched_at_unix_ms THEN excluded.alerts_json ELSE weather_snapshots_history.alerts_json END,
                     fetched_at_unix_ms = CASE WHEN excluded.fetched_at_unix_ms > weather_snapshots_history.fetched_at_unix_ms THEN excluded.fetched_at_unix_ms ELSE weather_snapshots_history.fetched_at_unix_ms END"#,
                params![uuid, new_date, snapshot_json, forecast_json, alerts_json, fetched_at],
            )?;
            tx.execute(
                "DELETE FROM weather_snapshots_history WHERE unified_uuid = ?1 AND date = ?2",
                params![uuid, old_date],
            )?;
            rewritten += 1;
        }
        tx.execute(
            "INSERT OR REPLACE INTO engine_state(key, value, updated_at_unix_ms) VALUES ('db_timezone', ?1, ?2)",
            params![new_tz, now_ms()],
        )?;
        tx.commit()?;
        Ok(rewritten)
    }

    pub(crate) fn log_fetch(
        &self,
        unified_uuid: Option<&str>,
        endpoint: &str,
        ok: bool,
        message: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO upstream_fetch_log(unified_uuid, endpoint, ok, message, fetched_at_unix_ms) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![unified_uuid, endpoint, if ok { 1 } else { 0 }, message, now_ms()],
        )?;
        Ok(())
    }

    /// 将 WAL 写回主 db 文件并截断 WAL,graceful shutdown 时调用。
    pub(crate) fn checkpoint(&self) -> Result<()> {
        self.conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
            .context("failed to checkpoint wal")?;
        Ok(())
    }
}

#[cfg(test)]
fn map_public_station_ref(row: &rusqlite::Row<'_>) -> rusqlite::Result<StationRef> {
    Ok(StationRef {
        province: row.get(0)?,
        city: row.get(1)?,
        name: row.get(2)?,
        unified_uuid: row.get(3)?,
    })
}

fn map_provider_station(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProviderStation> {
    Ok(ProviderStation {
        provider_name: row.get(0)?,
        display_name: row.get(1)?,
        provider_station_id: row.get(2)?,
        provider_province_code: row.get(3)?,
        province: row.get(4)?,
        city: row.get(5)?,
        url: row.get(6)?,
        name: row.get(7)?,
        unified_uuid: row.get(8)?,
    })
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use weather_schema::StationRef;

    fn temp_db() -> DbInstance {
        let path = std::env::temp_dir().join(format!(
            "weather-db-test-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);
        let db = DbInstance::open(path.clone(), "Asia/Shanghai").unwrap();
        std::fs::remove_file(path).ok();
        db
    }

    fn sample_station(name: &str) -> StationRef {
        StationRef {
            province: "北京市".to_string(),
            city: "朝阳".to_string(),
            name: name.to_string(),
            unified_uuid: unified_station_uuid(name),
        }
    }

    fn sample_snapshot(name: &str) -> WeatherSnapshot {
        WeatherSnapshot {
            station: Some(sample_station(name)),
            ..Default::default()
        }
    }

    fn sample_provider_station(name: &str) -> ProviderStation {
        ProviderStation {
            provider_name: "nmc".to_string(),
            display_name: name.to_string(),
            provider_station_id: "nmc-code".to_string(),
            provider_province_code: "ABJ".to_string(),
            province: "北京市".to_string(),
            city: "朝阳".to_string(),
            url: String::new(),
            name: name.to_string(),
            unified_uuid: unified_station_uuid(name),
        }
    }

    fn sample_provider_province(provider_code: &str, name: &str) -> ProviderProvince {
        ProviderProvince {
            provider_code: provider_code.to_string(),
            name: name.to_string(),
            url: format!("/publish/forecast/{provider_code}"),
        }
    }

    #[test]
    fn history_upsert_same_day_overwrites() {
        let db = temp_db();
        let uuid = unified_station_uuid("北京-北京市-朝阳");
        let snap1 = sample_snapshot("北京-北京市-朝阳");
        db.put_history_snapshot(&snap1, "f1", "a1", "2026-06-23")
            .unwrap();
        let snap2 = sample_snapshot("北京-北京市-朝阳");
        db.put_history_snapshot(&snap2, "f2", "a2", "2026-06-23")
            .unwrap();
        let stored = db
            .get_history_snapshot(&uuid, "2026-06-23")
            .unwrap()
            .unwrap();
        assert_eq!(
            stored.snapshot.station.as_ref().unwrap().name,
            "北京-北京市-朝阳"
        );
    }

    #[test]
    fn get_latest_returns_max_fetched() {
        let db = temp_db();
        let uuid = unified_station_uuid("北京-北京市-朝阳");
        db.put_history_snapshot(
            &sample_snapshot("北京-北京市-朝阳"),
            "f1",
            "a1",
            "2026-06-22",
        )
        .unwrap();
        // 第二行稍后写入，fetched_at_unix_ms 自然更大（now_ms 递增）
        db.put_history_snapshot(
            &sample_snapshot("北京-北京市-朝阳"),
            "f2",
            "a2",
            "2026-06-23",
        )
        .unwrap();
        let latest = db.get_latest_snapshot(&uuid).unwrap().unwrap();
        assert!(latest.fetched_at_unix_ms > 0);
    }

    #[test]
    fn get_latest_ignores_unreadable_snapshot_cache_row() {
        let db = temp_db();
        let uuid = unified_station_uuid("北京-北京市-朝阳");
        db.conn
            .execute(
                r#"INSERT INTO weather_snapshots_history(unified_uuid, date, station_name, snapshot_json, forecast_json, alerts_json, fetched_at_unix_ms)
                   VALUES (?1, '2026-06-29', '北京-北京市-朝阳', '{"climate":{"raw_json":"{}"}}', '{}', '{}', 1)"#,
                params![uuid],
            )
            .unwrap();

        let latest = db.get_latest_snapshot(&uuid).unwrap();

        assert!(latest.is_none());
    }

    #[test]
    fn timezone_first_init_writes_config_tz() {
        let path = std::env::temp_dir().join(format!(
            "weather-db-tz-init-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);
        {
            let db = DbInstance::open(path.clone(), "Asia/Shanghai").unwrap();
            assert_eq!(
                db.get_db_timezone().unwrap().as_deref(),
                Some("Asia/Shanghai")
            );
        }
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn timezone_mismatch_rejects_startup() {
        let path = std::env::temp_dir().join(format!(
            "weather-db-tz-mismatch-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);
        {
            let _db = DbInstance::open(path.clone(), "Asia/Shanghai").unwrap();
        }
        let err = match DbInstance::open(path.clone(), "UTC") {
            Ok(_) => panic!("expected timezone mismatch to reject startup"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("Asia/Shanghai"));
        assert!(err.to_string().contains("UTC"));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn migrate_timezone_rewrites_date() {
        let db = temp_db();
        let uuid = unified_station_uuid("北京-北京市-朝阳");
        let snap = sample_snapshot("北京-北京市-朝阳");
        let snap_json = serde_json::to_string(&snap).unwrap();
        // UTC 2026-06-23 22:00 = Shanghai 2026-06-24 06:00，按 Shanghai 算 date=06-24
        let fetched_at = DateTime::<Utc>::from_timestamp(1_782_252_000, 0)
            .unwrap()
            .timestamp_millis();
        db.conn
            .execute(
                r#"INSERT INTO weather_snapshots_history(unified_uuid, date, station_name, snapshot_json, forecast_json, alerts_json, fetched_at_unix_ms)
                   VALUES (?1, '2026-06-24', '北京-北京市-朝阳', ?2, '{}', '{}', ?3)"#,
                params![uuid, snap_json, fetched_at],
            )
            .unwrap();
        let rewritten = db.migrate_timezone("Asia/Shanghai", "UTC").unwrap();
        assert!(rewritten >= 1);
        let stored = db.get_history_snapshot(&uuid, "2026-06-24").unwrap();
        assert!(stored.is_none(), "old date row should be gone");
        let new_row = db.get_history_snapshot(&uuid, "2026-06-23").unwrap();
        assert!(new_row.is_some(), "new date row should exist");
        assert_eq!(db.get_db_timezone().unwrap().as_deref(), Some("UTC"));
    }

    #[test]
    fn public_station_by_uuid_round_trip() {
        let db = temp_db();
        let station = sample_station("北京-北京市-朝阳");
        db.put_public_station(&station).unwrap();
        let got = db
            .get_public_station_by_uuid(&station.unified_uuid)
            .unwrap()
            .unwrap();
        assert_eq!(got.name, "北京-北京市-朝阳");
        assert_eq!(got.unified_uuid, station.unified_uuid);
    }

    #[test]
    fn provider_station_mapping_round_trip_by_uuid() {
        let db = temp_db();
        let station = sample_provider_station("北京-北京市-朝阳");
        db.put_provider_station_mapping(&station).unwrap();

        let got = db
            .get_provider_station_by_uuid("nmc", &station.unified_uuid)
            .unwrap()
            .unwrap();

        assert_eq!(got.provider_station_id, "nmc-code");
        assert_eq!(got.provider_province_code, "ABJ");
        assert_eq!(got.public_ref().unified_uuid, station.unified_uuid);
    }

    #[test]
    fn provider_province_name_resolves_to_internal_code() {
        let db = temp_db();
        db.put_provider_provinces(&[sample_provider_province("ABJ", "北京市")])
            .unwrap();

        let got = db.resolve_provider_province_code("北京市").unwrap();

        assert_eq!(got, "ABJ");
    }

    #[test]
    fn provider_province_name_resolution_reports_missing_name() {
        let db = temp_db();

        let err = db.resolve_provider_province_code("不存在").unwrap_err();

        assert!(
            err.to_string()
                .contains("provider province `不存在` not found")
        );
    }

    #[test]
    fn provider_province_name_resolution_reports_ambiguous_name() {
        let db = temp_db();
        db.put_provider_provinces(&[
            sample_provider_province("AAA", "重复省"),
            sample_provider_province("BBB", "重复省"),
        ])
        .unwrap();

        let err = db.resolve_provider_province_code("重复省").unwrap_err();

        assert!(
            err.to_string()
                .contains("provider province `重复省` is ambiguous")
        );
    }

    #[test]
    fn fetch_log_uses_unified_uuid_column() {
        let db = temp_db();
        let uuid = unified_station_uuid("北京-北京市-朝阳");
        db.log_fetch(Some(&uuid), "rest/weather", true, None)
            .unwrap();

        let stored: String = db
            .conn
            .query_row(
                "SELECT unified_uuid FROM upstream_fetch_log WHERE endpoint = 'rest/weather'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, uuid);
    }
}
