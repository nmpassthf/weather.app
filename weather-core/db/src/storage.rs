use std::{
    path::{Path, PathBuf},
    str::FromStr,
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use weather_schema::{
    SCHEMA_VERSION, WeatherSnapshot, decode_message, encode_message, unified_station_uuid,
};

use crate::{
    actor::{CatalogCache, ProviderCity, ProviderProvince, ProviderStation, StoredSnapshot},
    migration,
    validation::{validate_provider_city_catalog, validate_provider_province_catalog},
};

pub(crate) const FETCH_LOG_RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1000;
pub(crate) const FETCH_LOG_MAX_ROWS: Option<u64> = Some(10_000);

const CATALOG_PROVINCES: &str = "provinces";
const CATALOG_CITIES: &str = "cities";
const PROVINCE_SCOPE: &str = "";
const DB_TIMEZONE_KEY: &str = "db_timezone";
const TIMEZONE_SYNC_PENDING_KEY: &str = "timezone_config_sync_pending";

pub(crate) struct DbInstance {
    conn: Connection,
    timezone: Tz,
}

impl DbInstance {
    pub(crate) fn open(path: PathBuf, config_tz: &str) -> Result<Self> {
        let timezone = parse_timezone(config_tz, "configured database timezone")?;
        let mut conn = Connection::open(path).context("failed to open sqlite database")?;
        conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            PRAGMA busy_timeout = 5000;
            "#,
        )
        .context("failed to configure sqlite connection")?;
        migration::migrate(&mut conn)?;
        let mut db = Self { conn, timezone };
        db.ensure_timezone(config_tz)?;
        Ok(db)
    }

    fn ensure_timezone(&mut self, config_tz: &str) -> Result<()> {
        let config_timezone = parse_timezone(config_tz, "configured database timezone")?;
        let stored = self.get_db_timezone()?;
        let pending = self.get_pending_timezone()?;
        if let Some(pending) = pending {
            let pending_timezone = parse_timezone(&pending, "pending database timezone")?;
            let stored_timezone = stored
                .as_deref()
                .context("database timezone metadata is missing while timezone sync is pending")
                .and_then(|stored| parse_timezone(stored, "stored database timezone"))?;
            if stored_timezone != pending_timezone || config_timezone != pending_timezone {
                bail!(
                    "timezone sync pending `{pending}` does not match DB `{}` and config `{config_tz}`",
                    stored.unwrap_or_else(|| "<missing>".to_string())
                );
            }
            self.timezone = pending_timezone;
            self.clear_pending_timezone(pending_timezone.name())?;
            return Ok(());
        }

        match stored {
            None => self.set_db_timezone(config_timezone.name()),
            Some(stored) => {
                let stored_timezone = parse_timezone(&stored, "stored database timezone")?;
                if stored_timezone != config_timezone {
                    bail!(
                        "DB timezone `{stored}` != config `{config_tz}`; call MIGRATE_DB_TIMEZONE RPC to migrate"
                    );
                }
                self.timezone = stored_timezone;
                Ok(())
            }
        }
    }

    pub(crate) fn put_history_snapshot(
        &mut self,
        snapshot: &WeatherSnapshot,
        fetched_at_unix_ms: i64,
    ) -> Result<()> {
        let fetched_at = DateTime::<Utc>::from_timestamp_millis(fetched_at_unix_ms)
            .with_context(|| format!("invalid fetched_at_unix_ms {fetched_at_unix_ms}"))?;
        let date = fetched_at
            .with_timezone(&self.timezone)
            .format("%Y-%m-%d")
            .to_string();
        let station = snapshot
            .station
            .as_ref()
            .context("weather snapshot is missing station")?;
        if station.name.trim().is_empty() {
            bail!("weather snapshot station name must not be empty");
        }
        if station.unified_uuid.trim().is_empty() {
            bail!("weather snapshot station unified_uuid must not be empty");
        }
        let canonical_uuid = unified_station_uuid(&station.name);
        if station.unified_uuid != canonical_uuid {
            bail!(
                "weather snapshot station unified_uuid `{}` is not canonical for `{}`",
                station.unified_uuid,
                station.name
            );
        }

        self.conn.execute(
            r#"INSERT INTO weather_snapshots_history(
                   unified_uuid, date, snapshot_schema_version, snapshot_proto, fetched_at_unix_ms
               ) VALUES (?1, ?2, ?3, ?4, ?5)
               ON CONFLICT(unified_uuid, date) DO UPDATE SET
                 snapshot_schema_version = excluded.snapshot_schema_version,
                 snapshot_proto = excluded.snapshot_proto,
                 fetched_at_unix_ms = excluded.fetched_at_unix_ms
               WHERE excluded.fetched_at_unix_ms >=
                     weather_snapshots_history.fetched_at_unix_ms"#,
            params![
                station.unified_uuid,
                date,
                SCHEMA_VERSION,
                encode_message(snapshot),
                fetched_at_unix_ms
            ],
        )?;
        Ok(())
    }

    pub(crate) fn get_latest_snapshot(&self, uuid: &str) -> Result<Option<StoredSnapshot>> {
        let row = self
            .conn
            .query_row(
                r#"SELECT date, snapshot_schema_version, snapshot_proto, fetched_at_unix_ms
                   FROM weather_snapshots_history
                   WHERE unified_uuid = ?1
                   ORDER BY fetched_at_unix_ms DESC
                   LIMIT 1"#,
                params![uuid],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?;
        let Some((date, schema_version, bytes, fetched_at_unix_ms)) = row else {
            return Ok(None);
        };
        if schema_version != SCHEMA_VERSION {
            bail!(
                "snapshot cache row {uuid}/{date} uses unsupported schema `{schema_version}`; expected `{SCHEMA_VERSION}`"
            );
        }
        let snapshot = decode_message(&bytes)
            .with_context(|| format!("failed to decode snapshot cache row {uuid}/{date}"))?;
        Ok(Some(StoredSnapshot {
            snapshot,
            fetched_at_unix_ms,
        }))
    }

    pub(crate) fn replace_provider_provinces(
        &mut self,
        provider: &str,
        provinces: &[ProviderProvince],
    ) -> Result<()> {
        validate_provider(provider)?;
        validate_provider_province_catalog(provinces)?;
        let now = now_ms();
        let row_count = i64::try_from(provinces.len()).context("province row count overflow")?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM provider_provinces WHERE provider = ?1",
            params![provider],
        )?;
        {
            let mut stmt = tx.prepare(
                r#"INSERT INTO provider_provinces(
                       provider, provider_code, name, url, updated_at_unix_ms
                   ) VALUES (?1, ?2, ?3, ?4, ?5)"#,
            )?;
            for province in provinces {
                stmt.execute(params![
                    provider,
                    province.provider_code,
                    province.name,
                    province.url,
                    now
                ])?;
            }
        }
        tx.execute(
            r#"DELETE FROM provider_cities
               WHERE provider = ?1
                 AND NOT EXISTS (
                     SELECT 1 FROM provider_provinces
                     WHERE provider_provinces.provider = provider_cities.provider
                       AND provider_provinces.provider_code = provider_cities.provider_province_code
                 )"#,
            params![provider],
        )?;
        tx.execute(
            r#"DELETE FROM catalog_cache_state
               WHERE provider = ?1 AND catalog_kind = 'cities'
                 AND NOT EXISTS (
                     SELECT 1 FROM provider_provinces
                     WHERE provider_provinces.provider = catalog_cache_state.provider
                       AND provider_provinces.provider_code = catalog_cache_state.scope
                 )"#,
            params![provider],
        )?;
        tx.execute(
            r#"DELETE FROM provider_station_mappings
               WHERE provider = ?1
                 AND NOT EXISTS (
                     SELECT 1 FROM provider_provinces
                     WHERE provider_provinces.provider = provider_station_mappings.provider
                       AND provider_provinces.provider_code = provider_station_mappings.provider_province_code
                 )"#,
            params![provider],
        )?;
        upsert_catalog_state(
            &tx,
            provider,
            CATALOG_PROVINCES,
            PROVINCE_SCOPE,
            now,
            row_count,
        )?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn get_provider_provinces(
        &self,
        provider: &str,
    ) -> Result<Option<CatalogCache<ProviderProvince>>> {
        validate_provider(provider)?;
        let Some((fetched_at_unix_ms, expected_count)) =
            self.get_catalog_state(provider, CATALOG_PROVINCES, PROVINCE_SCOPE)?
        else {
            return Ok(None);
        };
        let mut stmt = self.conn.prepare(
            r#"SELECT provider_code, name, url
               FROM provider_provinces
               WHERE provider = ?1
               ORDER BY provider_code"#,
        )?;
        let rows = stmt.query_map(params![provider], |row| {
            Ok(ProviderProvince {
                provider_code: row.get(0)?,
                name: row.get(1)?,
                url: row.get(2)?,
            })
        })?;
        let items = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        validate_catalog_count(
            provider,
            CATALOG_PROVINCES,
            PROVINCE_SCOPE,
            expected_count,
            items.len(),
        )?;
        Ok(Some(CatalogCache {
            items,
            fetched_at_unix_ms,
        }))
    }

    pub(crate) fn resolve_provider_province_code(
        &self,
        provider: &str,
        province: &str,
    ) -> Result<String> {
        validate_provider(provider)?;
        let mut stmt = self.conn.prepare(
            r#"SELECT provider_code FROM provider_provinces
               WHERE provider = ?1 AND name = ?2
               ORDER BY provider_code"#,
        )?;
        let rows = stmt
            .query_map(params![provider, province], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        match rows.as_slice() {
            [code] => Ok(code.clone()),
            [] => bail!("provider province `{province}` not found for `{provider}`"),
            codes => bail!(
                "provider province `{province}` is ambiguous for `{provider}`: {}",
                codes.join(", ")
            ),
        }
    }

    pub(crate) fn replace_provider_cities(
        &mut self,
        provider: &str,
        provider_province_code: &str,
        cities: &[ProviderCity],
    ) -> Result<()> {
        validate_provider(provider)?;
        validate_provider_city_catalog(provider_province_code, cities)?;
        let now = now_ms();
        let row_count = i64::try_from(cities.len()).context("city row count overflow")?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Station mappings are derived from this city scope.  Invalidate them
        // in the same transaction so a removed or remapped provider station ID
        // cannot outlive its authoritative catalog replacement.
        tx.execute(
            r#"DELETE FROM provider_station_mappings
               WHERE provider = ?1 AND provider_province_code = ?2"#,
            params![provider, provider_province_code],
        )?;
        tx.execute(
            "DELETE FROM provider_cities WHERE provider = ?1 AND provider_province_code = ?2",
            params![provider, provider_province_code],
        )?;
        {
            let mut stmt = tx.prepare(
                r#"INSERT INTO provider_cities(
                       provider, provider_code, provider_province_code,
                       province, city, url, updated_at_unix_ms
                   ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"#,
            )?;
            for city in cities {
                stmt.execute(params![
                    provider,
                    city.provider_code,
                    provider_province_code,
                    city.province,
                    city.city,
                    city.url,
                    now
                ])?;
            }
        }
        upsert_catalog_state(
            &tx,
            provider,
            CATALOG_CITIES,
            provider_province_code,
            now,
            row_count,
        )?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn get_provider_cities(
        &self,
        provider: &str,
        provider_province_code: &str,
    ) -> Result<Option<CatalogCache<ProviderCity>>> {
        validate_provider(provider)?;
        let Some((fetched_at_unix_ms, expected_count)) =
            self.get_catalog_state(provider, CATALOG_CITIES, provider_province_code)?
        else {
            return Ok(None);
        };
        let mut stmt = self.conn.prepare(
            r#"SELECT provider_code, provider_province_code, province, city, url
               FROM provider_cities
               WHERE provider = ?1 AND provider_province_code = ?2
               ORDER BY city, provider_code"#,
        )?;
        let rows = stmt.query_map(params![provider, provider_province_code], |row| {
            Ok(ProviderCity {
                provider_code: row.get(0)?,
                provider_province_code: row.get(1)?,
                province: row.get(2)?,
                city: row.get(3)?,
                url: row.get(4)?,
            })
        })?;
        let items = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        validate_catalog_count(
            provider,
            CATALOG_CITIES,
            provider_province_code,
            expected_count,
            items.len(),
        )?;
        Ok(Some(CatalogCache {
            items,
            fetched_at_unix_ms,
        }))
    }

    fn get_catalog_state(
        &self,
        provider: &str,
        kind: &str,
        scope: &str,
    ) -> Result<Option<(i64, i64)>> {
        self.conn
            .query_row(
                r#"SELECT fetched_at_unix_ms, row_count
                   FROM catalog_cache_state
                   WHERE provider = ?1 AND catalog_kind = ?2 AND scope = ?3"#,
                params![provider, kind, scope],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(Into::into)
    }

    pub(crate) fn put_provider_station_mapping(&mut self, station: &ProviderStation) -> Result<()> {
        validate_provider(&station.provider_name)?;
        if station.name.trim().is_empty() {
            bail!("provider station name must not be empty");
        }
        if station.unified_uuid.trim().is_empty() {
            bail!("provider station unified_uuid must not be empty");
        }
        let canonical_uuid = unified_station_uuid(&station.name);
        if station.unified_uuid != canonical_uuid {
            bail!(
                "provider station unified_uuid `{}` is not canonical for `{}`",
                station.unified_uuid,
                station.name
            );
        }
        self.conn.execute(
            r#"INSERT INTO provider_station_mappings(
                   provider, display_name, provider_station_id, provider_province_code,
                   province, city, url, name, unified_uuid, updated_at_unix_ms
               ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
               ON CONFLICT(provider, display_name) DO UPDATE SET
                 provider_station_id = excluded.provider_station_id,
                 provider_province_code = excluded.provider_province_code,
                 province = excluded.province,
                 city = excluded.city,
                 url = excluded.url,
                 name = excluded.name,
                 unified_uuid = excluded.unified_uuid,
                 updated_at_unix_ms = excluded.updated_at_unix_ms"#,
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
                r#"SELECT provider, display_name, provider_station_id, provider_province_code,
                          province, city, url, name, unified_uuid
                   FROM provider_station_mappings
                   WHERE provider = ?1 AND display_name = ?2"#,
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
                r#"SELECT provider, display_name, provider_station_id, provider_province_code,
                          province, city, url, name, unified_uuid
                   FROM provider_station_mappings
                   WHERE provider = ?1 AND unified_uuid = ?2
                   ORDER BY display_name LIMIT 1"#,
                params![provider, uuid],
                map_provider_station,
            )
            .optional()
            .map_err(Into::into)
    }

    pub(crate) fn get_db_timezone(&self) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT value FROM engine_state WHERE key = ?1",
                params![DB_TIMEZONE_KEY],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(Into::into)
    }

    fn set_db_timezone(&mut self, timezone: &str) -> Result<()> {
        let timezone = parse_timezone(timezone, "database timezone")?;
        self.conn.execute(
            r#"INSERT INTO engine_state(key, value, updated_at_unix_ms)
               VALUES (?1, ?2, ?3)
               ON CONFLICT(key) DO UPDATE SET
                 value = excluded.value,
                 updated_at_unix_ms = excluded.updated_at_unix_ms"#,
            params![DB_TIMEZONE_KEY, timezone.name(), now_ms()],
        )?;
        self.timezone = timezone;
        Ok(())
    }

    pub(crate) fn migrate_timezone(&mut self, old_tz: &str, new_tz: &str) -> Result<u64> {
        let old_timezone = parse_timezone(old_tz, "old database timezone")?;
        let new_timezone = parse_timezone(new_tz, "new database timezone")?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let pending = tx
            .query_row(
                "SELECT value FROM engine_state WHERE key = ?1",
                params![TIMEZONE_SYNC_PENDING_KEY],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(pending) = pending {
            bail!("timezone config sync to `{pending}` is already pending");
        }
        let stored = tx
            .query_row(
                "SELECT value FROM engine_state WHERE key = ?1",
                params![DB_TIMEZONE_KEY],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .context("database timezone metadata is missing")?;
        let stored_timezone = parse_timezone(&stored, "stored database timezone")?;
        if stored_timezone != old_timezone {
            bail!("DB timezone `{stored}` does not match requested old timezone `{old_tz}`");
        }
        if old_timezone == new_timezone {
            tx.commit()?;
            self.timezone = new_timezone;
            return Ok(0);
        }

        migration::validate_history_rebuild_schema(&tx)?;

        // Keep only small key/timestamp metadata in Rust.  Snapshot protobufs
        // are copied inside SQLite below, avoiding a full-history BLOB Vec and
        // its corresponding memory spike.
        let entries = {
            let mut stmt = tx.prepare(
                r#"SELECT unified_uuid, date, fetched_at_unix_ms
                   FROM weather_snapshots_history
                   ORDER BY unified_uuid, date"#,
            )?;
            stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
        };

        tx.execute_batch(
            r#"
            DROP TABLE IF EXISTS temp.weather_history_timezone_map;
            CREATE TEMP TABLE weather_history_timezone_map(
                unified_uuid TEXT NOT NULL,
                old_date TEXT NOT NULL,
                new_date TEXT NOT NULL,
                fetched_at_unix_ms INTEGER NOT NULL,
                PRIMARY KEY(unified_uuid, old_date)
            );
            CREATE INDEX temp.idx_weather_history_timezone_target
                ON weather_history_timezone_map(
                    unified_uuid, new_date, fetched_at_unix_ms DESC, old_date DESC
                );
            "#,
        )?;

        let mut rewritten = 0u64;
        {
            let mut stmt = tx.prepare(
                r#"INSERT INTO temp.weather_history_timezone_map(
                       unified_uuid, old_date, new_date, fetched_at_unix_ms
                   ) VALUES (?1, ?2, ?3, ?4)"#,
            )?;
            for (uuid, old_date, fetched_at) in entries {
                let dt = DateTime::<Utc>::from_timestamp_millis(fetched_at)
                    .with_context(|| format!("invalid fetched_at_unix_ms {fetched_at}"))?;
                let new_date = dt
                    .with_timezone(&new_timezone)
                    .format("%Y-%m-%d")
                    .to_string();
                rewritten += u64::from(new_date != old_date);
                stmt.execute(params![uuid, old_date, new_date, fetched_at])?;
            }
        }

        let expected_count: i64 = tx.query_row(
            r#"SELECT COUNT(*)
               FROM (
                   SELECT unified_uuid, new_date
                   FROM temp.weather_history_timezone_map
                   GROUP BY unified_uuid, new_date
               )"#,
            [],
            |row| row.get(0),
        )?;

        tx.execute_batch(&format!(
            "ALTER TABLE {} RENAME TO {};",
            migration::HISTORY_TABLE_NAME,
            migration::HISTORY_OLD_TABLE_NAME
        ))?;
        migration::create_history_table(&tx)?;
        tx.execute_batch(&format!(
            r#"INSERT INTO {history}(
                   unified_uuid, date, snapshot_schema_version,
                   snapshot_proto, fetched_at_unix_ms
               )
               SELECT source.unified_uuid,
                      mapping.new_date,
                      source.snapshot_schema_version,
                      source.snapshot_proto,
                      source.fetched_at_unix_ms
               FROM {old} AS source
               JOIN temp.weather_history_timezone_map AS mapping
                 ON mapping.unified_uuid = source.unified_uuid
                AND mapping.old_date = source.date
               WHERE NOT EXISTS (
                   SELECT 1
                   FROM temp.weather_history_timezone_map AS newer
                   WHERE newer.unified_uuid = mapping.unified_uuid
                     AND newer.new_date = mapping.new_date
                     AND (
                         newer.fetched_at_unix_ms > mapping.fetched_at_unix_ms
                         OR (
                             newer.fetched_at_unix_ms = mapping.fetched_at_unix_ms
                             AND newer.old_date > mapping.old_date
                         )
                     )
               );"#,
            history = migration::HISTORY_TABLE_NAME,
            old = migration::HISTORY_OLD_TABLE_NAME,
        ))?;
        let staged_count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM weather_snapshots_history",
            [],
            |row| row.get(0),
        )?;
        if staged_count != expected_count {
            bail!(
                "timezone migration staged {staged_count} history rows; expected {expected_count}"
            );
        }

        tx.execute_batch(&format!(
            "DROP TABLE {}; DROP TABLE temp.weather_history_timezone_map;",
            migration::HISTORY_OLD_TABLE_NAME
        ))?;
        migration::create_history_index(&tx)?;
        tx.execute(
            r#"INSERT INTO engine_state(key, value, updated_at_unix_ms)
               VALUES (?1, ?2, ?3)
               ON CONFLICT(key) DO UPDATE SET
                 value = excluded.value,
                 updated_at_unix_ms = excluded.updated_at_unix_ms"#,
            params![DB_TIMEZONE_KEY, new_timezone.name(), now_ms()],
        )?;
        tx.execute(
            r#"INSERT INTO engine_state(key, value, updated_at_unix_ms)
               VALUES (?1, ?2, ?3)
               ON CONFLICT(key) DO UPDATE SET
                 value = excluded.value,
                 updated_at_unix_ms = excluded.updated_at_unix_ms"#,
            params![TIMEZONE_SYNC_PENDING_KEY, new_timezone.name(), now_ms()],
        )?;
        tx.commit()?;
        self.timezone = new_timezone;
        Ok(rewritten)
    }

    pub(crate) fn get_pending_timezone(&self) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT value FROM engine_state WHERE key = ?1",
                params![TIMEZONE_SYNC_PENDING_KEY],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub(crate) fn clear_pending_timezone(&mut self, expected: &str) -> Result<()> {
        let expected = parse_timezone(expected, "expected pending database timezone")?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let pending = tx
            .query_row(
                "SELECT value FROM engine_state WHERE key = ?1",
                params![TIMEZONE_SYNC_PENDING_KEY],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(pending) = pending {
            let pending_timezone = parse_timezone(&pending, "pending database timezone")?;
            if pending_timezone != expected {
                bail!(
                    "pending database timezone `{pending}` does not match expected `{}`",
                    expected.name()
                );
            }
            tx.execute(
                "DELETE FROM engine_state WHERE key = ?1",
                params![TIMEZONE_SYNC_PENDING_KEY],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn log_fetch(
        &mut self,
        unified_uuid: Option<&str>,
        endpoint: &str,
        ok: bool,
        message: Option<&str>,
    ) -> Result<()> {
        let now = now_ms();
        let tx = self.conn.transaction()?;
        tx.execute(
            r#"INSERT INTO upstream_fetch_log(
                   unified_uuid, endpoint, ok, message, fetched_at_unix_ms
               ) VALUES (?1, ?2, ?3, ?4, ?5)"#,
            params![unified_uuid, endpoint, i64::from(ok), message, now],
        )?;
        prune_fetch_logs(&tx, now, FETCH_LOG_RETENTION_MS, FETCH_LOG_MAX_ROWS)?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn checkpoint(&self) -> Result<()> {
        self.conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
            .context("failed to checkpoint wal")?;
        Ok(())
    }
}

pub(crate) fn inspect_pending_timezone(path: &Path) -> Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("failed to inspect database {}", path.display()))?;
    let has_engine_state: bool = conn.query_row(
        r#"SELECT EXISTS(
               SELECT 1 FROM sqlite_schema
               WHERE type = 'table' AND name = 'engine_state'
           )"#,
        [],
        |row| row.get(0),
    )?;
    if !has_engine_state {
        return Ok(None);
    }
    let pending = conn
        .query_row(
            "SELECT value FROM engine_state WHERE key = ?1",
            params![TIMEZONE_SYNC_PENDING_KEY],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let Some(pending) = pending else {
        return Ok(None);
    };
    let stored = conn
        .query_row(
            "SELECT value FROM engine_state WHERE key = ?1",
            params![DB_TIMEZONE_KEY],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .context("database timezone metadata is missing while timezone sync is pending")?;
    let pending_timezone = parse_timezone(&pending, "pending database timezone")?;
    let stored_timezone = parse_timezone(&stored, "stored database timezone")?;
    if pending_timezone != stored_timezone {
        bail!("timezone sync pending `{pending}` does not match DB `{stored}`");
    }
    Ok(Some(pending_timezone.name().to_string()))
}

fn upsert_catalog_state(
    conn: &Connection,
    provider: &str,
    kind: &str,
    scope: &str,
    fetched_at_unix_ms: i64,
    row_count: i64,
) -> Result<()> {
    conn.execute(
        r#"INSERT INTO catalog_cache_state(
               provider, catalog_kind, scope, fetched_at_unix_ms, row_count
           ) VALUES (?1, ?2, ?3, ?4, ?5)
           ON CONFLICT(provider, catalog_kind, scope) DO UPDATE SET
             fetched_at_unix_ms = excluded.fetched_at_unix_ms,
             row_count = excluded.row_count"#,
        params![provider, kind, scope, fetched_at_unix_ms, row_count],
    )?;
    Ok(())
}

fn validate_provider(provider: &str) -> Result<()> {
    if provider.trim().is_empty() {
        bail!("provider must not be empty");
    }
    Ok(())
}

fn parse_timezone(value: &str, label: &str) -> Result<Tz> {
    Tz::from_str(value).map_err(|_| anyhow::anyhow!("invalid {label} `{value}`"))
}

fn validate_catalog_count(
    provider: &str,
    kind: &str,
    scope: &str,
    expected: i64,
    actual: usize,
) -> Result<()> {
    let actual = i64::try_from(actual).context("catalog row count overflow")?;
    if expected != actual {
        bail!(
            "catalog cache state mismatch for provider `{provider}` kind `{kind}` scope `{scope}`: expected {expected} rows, found {actual}"
        );
    }
    Ok(())
}

fn prune_fetch_logs(
    conn: &Connection,
    now_unix_ms: i64,
    retention_ms: i64,
    max_rows: Option<u64>,
) -> Result<()> {
    let cutoff = now_unix_ms.saturating_sub(retention_ms.max(0));
    conn.execute(
        "DELETE FROM upstream_fetch_log WHERE fetched_at_unix_ms < ?1",
        params![cutoff],
    )?;
    if let Some(max_rows) = max_rows {
        let offset = i64::try_from(max_rows).context("fetch log row limit exceeds i64")?;
        conn.execute(
            r#"DELETE FROM upstream_fetch_log
               WHERE id IN (
                   SELECT id FROM upstream_fetch_log
                   ORDER BY fetched_at_unix_ms DESC, id DESC
                   LIMIT -1 OFFSET ?1
               )"#,
            params![offset],
        )?;
    }
    Ok(())
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

    fn temp_db_with_timezone(timezone: &str) -> DbInstance {
        DbInstance::open(PathBuf::from(":memory:"), timezone).unwrap()
    }

    fn temp_db() -> DbInstance {
        temp_db_with_timezone("Asia/Shanghai")
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

    fn sample_province(code: &str, name: &str) -> ProviderProvince {
        ProviderProvince {
            provider_code: code.to_string(),
            name: name.to_string(),
            url: format!("/publish/forecast/{code}"),
        }
    }

    fn sample_city(code: &str, province_code: &str, city: &str) -> ProviderCity {
        ProviderCity {
            provider_code: code.to_string(),
            provider_province_code: province_code.to_string(),
            province: "测试省".to_string(),
            city: city.to_string(),
            url: format!("/{code}"),
        }
    }

    fn sample_provider_station(
        provider: &str,
        province_code: &str,
        provider_station_id: &str,
    ) -> ProviderStation {
        let name = format!("测试-{province_code}-{provider_station_id}");
        ProviderStation {
            provider_name: provider.to_string(),
            display_name: name.clone(),
            provider_station_id: provider_station_id.to_string(),
            provider_province_code: province_code.to_string(),
            province: "测试省".to_string(),
            city: provider_station_id.to_string(),
            url: format!("/{provider_station_id}"),
            unified_uuid: unified_station_uuid(&name),
            name,
        }
    }

    fn history_schema_signature(db: &DbInstance) -> Vec<(String, String, String)> {
        db.conn
            .prepare(
                r#"SELECT type, name, coalesce(sql, '')
                   FROM sqlite_schema
                   WHERE tbl_name = 'weather_snapshots_history'
                   ORDER BY type, name"#,
            )
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap()
    }

    fn insert_history_row(
        db: &DbInstance,
        uuid: &str,
        date: &str,
        snapshot: &WeatherSnapshot,
        fetched_at: i64,
    ) {
        db.conn
            .execute(
                r#"INSERT INTO weather_snapshots_history(
                       unified_uuid, date, snapshot_schema_version,
                       snapshot_proto, fetched_at_unix_ms
                   ) VALUES (?1, ?2, ?3, ?4, ?5)"#,
                params![
                    uuid,
                    date,
                    SCHEMA_VERSION,
                    encode_message(snapshot),
                    fetched_at
                ],
            )
            .unwrap();
    }

    #[test]
    fn protobuf_history_round_trips_and_rejects_bad_uuid() {
        let mut db = temp_db();
        let snapshot = sample_snapshot("北京-北京市-朝阳");
        let uuid = snapshot.station.as_ref().unwrap().unified_uuid.clone();
        let fetched_at = DateTime::parse_from_rfc3339("2026-06-23T23:00:00Z")
            .unwrap()
            .timestamp_millis();
        db.put_history_snapshot(&snapshot, fetched_at).unwrap();
        let stored = db.get_latest_snapshot(&uuid).unwrap().unwrap();
        assert_eq!(stored.snapshot.station.unwrap().unified_uuid, uuid);
        assert_eq!(stored.fetched_at_unix_ms, fetched_at);
        let stored_date: String = db
            .conn
            .query_row(
                "SELECT date FROM weather_snapshots_history WHERE unified_uuid = ?1",
                params![uuid],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored_date, "2026-06-24");

        let mut empty = snapshot.clone();
        empty.station.as_mut().unwrap().unified_uuid.clear();
        assert!(db.put_history_snapshot(&empty, fetched_at).is_err());

        let mut wrong = snapshot;
        wrong.station.as_mut().unwrap().unified_uuid = "not-canonical".to_string();
        assert!(db.put_history_snapshot(&wrong, fetched_at).is_err());
        assert!(db.put_history_snapshot(&wrong, i64::MAX).is_err());
    }

    #[test]
    fn persisted_protobuf_fixture_remains_wire_compatible() {
        const NAME: &str = "fixture";
        const UUID: &str = "3c78571a-ca24-58e6-a804-266dbda1eaa8";
        // Hard-coded weather.schema.v1 bytes, intentionally not produced by the
        // generated encoder in this test:
        // WeatherSnapshot { station: { province: "P", city: "C",
        // name: "fixture", unified_uuid: UUID }, stale: true }.
        const SNAPSHOT_PROTO: &[u8] = b"\x0a\x35\x0a\x01P\x12\x01C\x1a\x07fixture\x22\x243c78571a-ca24-58e6-a804-266dbda1eaa8\x48\x01";

        assert_eq!(unified_station_uuid(NAME), UUID);
        let db = temp_db();
        db.conn
            .execute(
                r#"INSERT INTO weather_snapshots_history(
                       unified_uuid, date, snapshot_schema_version,
                       snapshot_proto, fetched_at_unix_ms
                   ) VALUES (?1, '2026-06-23', 'weather.schema.v1', ?2, 42)"#,
                params![UUID, SNAPSHOT_PROTO],
            )
            .unwrap();

        let stored = db.get_latest_snapshot(UUID).unwrap().unwrap();
        let station = stored.snapshot.station.unwrap();
        assert_eq!(station.province, "P");
        assert_eq!(station.city, "C");
        assert_eq!(station.name, NAME);
        assert_eq!(station.unified_uuid, UUID);
        assert!(stored.snapshot.stale);
        assert_eq!(stored.fetched_at_unix_ms, 42);
    }

    #[test]
    fn older_same_day_snapshot_cannot_overwrite_newer_history() {
        let mut db = temp_db_with_timezone("UTC");
        let name = "北京-北京市-朝阳";
        let uuid = unified_station_uuid(name);
        let older_at = DateTime::parse_from_rfc3339("2026-06-23T01:00:00Z")
            .unwrap()
            .timestamp_millis();
        let newer_at = DateTime::parse_from_rfc3339("2026-06-23T02:00:00Z")
            .unwrap()
            .timestamp_millis();
        let mut older = sample_snapshot(name);
        older.stale = true;
        let newer = sample_snapshot(name);

        db.put_history_snapshot(&newer, newer_at).unwrap();
        db.put_history_snapshot(&older, older_at).unwrap();

        let stored = db.get_latest_snapshot(&uuid).unwrap().unwrap();
        assert_eq!(stored.fetched_at_unix_ms, newer_at);
        assert!(!stored.snapshot.stale);
    }

    #[test]
    fn corrupted_protobuf_reports_row_identity() {
        let db = temp_db();
        let uuid = unified_station_uuid("北京-北京市-朝阳");
        db.conn
            .execute(
                r#"INSERT INTO weather_snapshots_history(
                       unified_uuid, date, snapshot_schema_version,
                       snapshot_proto, fetched_at_unix_ms
                   ) VALUES (?1, '2026-06-23', ?2, X'FFFF', 1)"#,
                params![uuid, SCHEMA_VERSION],
            )
            .unwrap();

        let err = db.get_latest_snapshot(&uuid).unwrap_err().to_string();
        assert!(err.contains(&uuid), "{err}");
        assert!(err.contains("2026-06-23"), "{err}");
    }

    #[test]
    fn provider_catalogs_are_isolated_and_empty_is_cached() {
        let mut db = temp_db();
        db.replace_provider_provinces("nmc", &[sample_province("X", "NMC")])
            .unwrap();
        db.replace_provider_provinces("other", &[sample_province("X", "Other")])
            .unwrap();
        assert_eq!(
            db.get_provider_provinces("nmc").unwrap().unwrap().items[0].name,
            "NMC"
        );
        assert_eq!(
            db.get_provider_provinces("other").unwrap().unwrap().items[0].name,
            "Other"
        );

        db.replace_provider_cities("nmc", "X", &[]).unwrap();
        let empty = db.get_provider_cities("nmc", "X").unwrap().unwrap();
        assert!(empty.items.is_empty());
        assert!(empty.fetched_at_unix_ms > 0);
        assert!(db.get_provider_cities("other", "X").unwrap().is_none());
    }

    #[test]
    fn replace_removes_stale_rows_and_orphaned_city_scopes() {
        let mut db = temp_db();
        db.replace_provider_provinces(
            "nmc",
            &[sample_province("A", "A"), sample_province("B", "B")],
        )
        .unwrap();
        db.replace_provider_cities("nmc", "A", &[sample_city("A1", "A", "A1")])
            .unwrap();
        db.replace_provider_cities(
            "nmc",
            "B",
            &[sample_city("B1", "B", "B1"), sample_city("B2", "B", "B2")],
        )
        .unwrap();
        db.replace_provider_cities("nmc", "B", &[sample_city("B2", "B", "B2")])
            .unwrap();
        assert_eq!(
            db.get_provider_cities("nmc", "B")
                .unwrap()
                .unwrap()
                .items
                .len(),
            1
        );

        db.replace_provider_provinces("nmc", &[sample_province("B", "B")])
            .unwrap();
        assert!(db.get_provider_cities("nmc", "A").unwrap().is_none());
        assert!(db.resolve_provider_province_code("nmc", "A").is_err());
    }

    #[test]
    fn catalog_replace_invalidates_derived_station_mappings_in_the_same_scope() {
        let mut db = temp_db();
        db.replace_provider_provinces(
            "nmc",
            &[sample_province("A", "A"), sample_province("B", "B")],
        )
        .unwrap();
        db.replace_provider_cities("nmc", "A", &[sample_city("A1", "A", "A1")])
            .unwrap();
        db.replace_provider_cities("nmc", "B", &[sample_city("B1", "B", "B1")])
            .unwrap();
        let station_a = sample_provider_station("nmc", "A", "A1");
        let station_b = sample_provider_station("nmc", "B", "B1");
        let other = sample_provider_station("other", "A", "OTHER");
        db.put_provider_station_mapping(&station_a).unwrap();
        db.put_provider_station_mapping(&station_b).unwrap();
        db.put_provider_station_mapping(&other).unwrap();

        db.replace_provider_cities("nmc", "B", &[sample_city("B2", "B", "B2")])
            .unwrap();
        assert!(
            db.get_provider_station_by_name("nmc", &station_b.display_name)
                .unwrap()
                .is_none()
        );
        assert!(
            db.get_provider_station_by_name("nmc", &station_a.display_name)
                .unwrap()
                .is_some()
        );

        db.put_provider_station_mapping(&station_b).unwrap();
        db.replace_provider_provinces("nmc", &[sample_province("B", "B")])
            .unwrap();
        assert!(
            db.get_provider_station_by_name("nmc", &station_a.display_name)
                .unwrap()
                .is_none()
        );
        assert!(
            db.get_provider_station_by_name("nmc", &station_b.display_name)
                .unwrap()
                .is_some()
        );
        assert!(
            db.get_provider_station_by_name("other", &other.display_name)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn failed_replace_rolls_back_rows_and_cache_state() {
        let mut db = temp_db();
        db.replace_provider_provinces("nmc", &[sample_province("OLD", "Old")])
            .unwrap();
        let old_timestamp = db
            .get_provider_provinces("nmc")
            .unwrap()
            .unwrap()
            .fetched_at_unix_ms;
        db.conn
            .execute_batch(
                r#"CREATE TRIGGER fail_provider_insert
                   BEFORE INSERT ON provider_provinces
                   WHEN NEW.provider_code = 'FAIL'
                   BEGIN SELECT RAISE(ABORT, 'injected'); END;"#,
            )
            .unwrap();

        let result = db.replace_provider_provinces(
            "nmc",
            &[
                sample_province("GOOD", "Good"),
                sample_province("FAIL", "Fail"),
            ],
        );
        assert!(result.is_err());
        let cached = db.get_provider_provinces("nmc").unwrap().unwrap();
        assert_eq!(cached.items[0].provider_code, "OLD");
        assert_eq!(cached.fetched_at_unix_ms, old_timestamp);
    }

    #[test]
    fn timezone_staging_preserves_chained_moves() {
        let mut db = temp_db_with_timezone("UTC");
        let name = "北京-北京市-朝阳";
        let uuid = unified_station_uuid(name);
        let snapshot = sample_snapshot(name);
        let first = DateTime::parse_from_rfc3339("2026-06-23T23:00:00Z")
            .unwrap()
            .timestamp_millis();
        let second = DateTime::parse_from_rfc3339("2026-06-24T23:00:00Z")
            .unwrap()
            .timestamp_millis();
        insert_history_row(&db, &uuid, "2026-06-23", &snapshot, first);
        insert_history_row(&db, &uuid, "2026-06-24", &snapshot, second);

        assert_eq!(db.migrate_timezone("UTC", "Asia/Shanghai").unwrap(), 2);
        let dates: Vec<String> = db
            .conn
            .prepare("SELECT date FROM weather_snapshots_history ORDER BY date")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(dates, vec!["2026-06-24", "2026-06-25"]);

        let after_migration = sample_snapshot("迁移后站点");
        let after_uuid = after_migration
            .station
            .as_ref()
            .unwrap()
            .unified_uuid
            .clone();
        db.put_history_snapshot(&after_migration, first).unwrap();
        let date: String = db
            .conn
            .query_row(
                "SELECT date FROM weather_snapshots_history WHERE unified_uuid = ?1",
                params![after_uuid],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(date, "2026-06-24");
    }

    #[test]
    fn timezone_rebuild_preserves_the_canonical_history_schema() {
        let mut db = temp_db_with_timezone("UTC");
        let before = history_schema_signature(&db);
        let snapshot = sample_snapshot("北京-北京市-朝阳");
        let fetched_at = DateTime::parse_from_rfc3339("2026-06-23T23:00:00Z")
            .unwrap()
            .timestamp_millis();
        db.put_history_snapshot(&snapshot, fetched_at).unwrap();

        db.migrate_timezone("UTC", "Asia/Shanghai").unwrap();

        assert_eq!(history_schema_signature(&db), before);
    }

    #[test]
    fn timezone_rebuild_rejects_extra_history_schema_objects_without_changes() {
        let mut db = temp_db_with_timezone("UTC");
        let name = "北京-北京市-朝阳";
        let uuid = unified_station_uuid(name);
        let fetched_at = DateTime::parse_from_rfc3339("2026-06-23T23:00:00Z")
            .unwrap()
            .timestamp_millis();
        insert_history_row(&db, &uuid, "2026-06-23", &sample_snapshot(name), fetched_at);
        db.conn
            .execute(
                "CREATE INDEX extra_history_date ON weather_snapshots_history(date)",
                [],
            )
            .unwrap();

        let err = db
            .migrate_timezone("UTC", "Asia/Shanghai")
            .unwrap_err()
            .to_string();

        assert!(err.contains("extra schema objects"), "{err}");
        assert_eq!(db.get_db_timezone().unwrap().as_deref(), Some("UTC"));
        assert_eq!(db.get_pending_timezone().unwrap(), None);
        let date: String = db
            .conn
            .query_row("SELECT date FROM weather_snapshots_history", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(date, "2026-06-23");
    }

    #[test]
    fn timezone_inputs_are_validated_before_state_changes() {
        assert!(DbInstance::open(PathBuf::from(":memory:"), "Not/A/Zone").is_err());

        let mut db = temp_db_with_timezone("UTC");
        assert!(db.set_db_timezone("Not/A/Zone").is_err());
        assert_eq!(db.get_db_timezone().unwrap().as_deref(), Some("UTC"));
        assert!(db.migrate_timezone("Not/A/Zone", "Asia/Shanghai").is_err());
        assert_eq!(db.get_db_timezone().unwrap().as_deref(), Some("UTC"));
    }

    #[test]
    fn timezone_pending_marker_is_written_only_for_real_migration() {
        let mut db = temp_db_with_timezone("UTC");

        assert_eq!(db.migrate_timezone("UTC", "UTC").unwrap(), 0);
        assert_eq!(db.get_pending_timezone().unwrap(), None);

        db.migrate_timezone("UTC", "Asia/Shanghai").unwrap();
        assert_eq!(
            db.get_pending_timezone().unwrap().as_deref(),
            Some("Asia/Shanghai")
        );
        assert_eq!(db.timezone, chrono_tz::Asia::Shanghai);
        db.clear_pending_timezone("Asia/Shanghai").unwrap();
        assert_eq!(db.get_pending_timezone().unwrap(), None);
    }

    #[test]
    fn pending_timezone_inspection_reads_committed_wal_state() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("weather.db");
        let mut db = DbInstance::open(path.clone(), "UTC").unwrap();

        db.migrate_timezone("UTC", "Asia/Shanghai").unwrap();

        // Keep the writer open and do not checkpoint: startup recovery must be
        // able to observe a committed marker that still resides in the WAL.
        assert_eq!(
            inspect_pending_timezone(&path).unwrap().as_deref(),
            Some("Asia/Shanghai")
        );
    }

    #[test]
    fn mismatched_pending_timezone_is_rejected_before_recovery() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("weather.db");
        {
            let db = DbInstance::open(path.clone(), "UTC").unwrap();
            db.conn
                .execute(
                    r#"INSERT INTO engine_state(key, value, updated_at_unix_ms)
                       VALUES (?1, 'Asia/Shanghai', ?2)"#,
                    params![TIMEZONE_SYNC_PENDING_KEY, now_ms()],
                )
                .unwrap();
        }

        assert!(inspect_pending_timezone(&path).is_err());
        assert!(DbInstance::open(path, "Asia/Shanghai").is_err());
    }

    #[test]
    fn timezone_collision_keeps_latest_complete_snapshot() {
        let mut db = temp_db_with_timezone("America/Adak");
        let name = "北京-北京市-朝阳";
        let uuid = unified_station_uuid(name);
        let mut older = sample_snapshot(name);
        older.stale = true;
        let newer = sample_snapshot(name);
        let older_at = DateTime::parse_from_rfc3339("2026-06-24T08:00:00Z")
            .unwrap()
            .timestamp_millis();
        let newer_at = DateTime::parse_from_rfc3339("2026-06-24T10:00:00Z")
            .unwrap()
            .timestamp_millis();
        insert_history_row(&db, &uuid, "2026-06-23", &older, older_at);
        insert_history_row(&db, &uuid, "2026-06-24", &newer, newer_at);

        assert_eq!(db.migrate_timezone("America/Adak", "UTC").unwrap(), 1);
        let stored = db.get_latest_snapshot(&uuid).unwrap().unwrap();
        assert_eq!(stored.fetched_at_unix_ms, newer_at);
        assert!(!stored.snapshot.stale);
        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM weather_snapshots_history",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn timezone_failure_rolls_back_table_and_metadata() {
        let mut db = temp_db_with_timezone("UTC");
        let name = "北京-北京市-朝阳";
        let uuid = unified_station_uuid(name);
        let fetched_at = DateTime::parse_from_rfc3339("2026-06-23T23:00:00Z")
            .unwrap()
            .timestamp_millis();
        insert_history_row(&db, &uuid, "2026-06-23", &sample_snapshot(name), fetched_at);
        db.conn
            .execute_batch(
                r#"CREATE TRIGGER fail_timezone_update
                   BEFORE UPDATE ON engine_state
                   WHEN NEW.key = 'db_timezone'
                   BEGIN SELECT RAISE(ABORT, 'injected'); END;"#,
            )
            .unwrap();

        assert!(db.migrate_timezone("UTC", "Asia/Shanghai").is_err());
        assert_eq!(db.get_db_timezone().unwrap().as_deref(), Some("UTC"));
        let date: String = db
            .conn
            .query_row("SELECT date FROM weather_snapshots_history", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(date, "2026-06-23");
    }

    #[test]
    fn fetch_log_retention_prunes_by_age_and_optional_row_limit() {
        let mut db = temp_db();
        let now = now_ms();
        for (index, timestamp) in [now - FETCH_LOG_RETENTION_MS - 1, now - 2, now - 1, now]
            .into_iter()
            .enumerate()
        {
            db.conn
                .execute(
                    r#"INSERT INTO upstream_fetch_log(
                           endpoint, ok, fetched_at_unix_ms
                       ) VALUES (?1, 1, ?2)"#,
                    params![format!("endpoint-{index}"), timestamp],
                )
                .unwrap();
        }
        prune_fetch_logs(&db.conn, now, FETCH_LOG_RETENTION_MS, Some(2)).unwrap();
        let endpoints: Vec<String> = db
            .conn
            .prepare("SELECT endpoint FROM upstream_fetch_log ORDER BY fetched_at_unix_ms")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(endpoints, vec!["endpoint-2", "endpoint-3"]);

        db.log_fetch(None, "current", true, None).unwrap();
    }
}
