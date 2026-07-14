use anyhow::{Context, Result, bail};
use rusqlite::{Connection, Transaction, TransactionBehavior, params};
use weather_schema::unix_timestamp_ms;

pub(crate) const LATEST_DB_VERSION: i64 = 1;
pub(crate) const HISTORY_TABLE_NAME: &str = "weather_snapshots_history";
pub(crate) const HISTORY_OLD_TABLE_NAME: &str = "weather_snapshots_history__tz_old";
pub(crate) const HISTORY_INDEX_NAME: &str = "idx_history_uuid_fetched";

const HISTORY_TABLE_SQL: &str = r#"CREATE TABLE weather_snapshots_history(
    unified_uuid TEXT NOT NULL CHECK(length(trim(unified_uuid)) > 0),
    date TEXT NOT NULL,
    snapshot_schema_version TEXT NOT NULL,
    snapshot_proto BLOB NOT NULL,
    fetched_at_unix_ms INTEGER NOT NULL,
    PRIMARY KEY(unified_uuid, date)
)"#;
const HISTORY_INDEX_SQL: &str = r#"CREATE INDEX idx_history_uuid_fetched
    ON weather_snapshots_history(unified_uuid, fetched_at_unix_ms DESC)"#;

#[derive(Clone, Copy)]
struct Migration {
    version: i64,
    name: &'static str,
    apply: fn(&Transaction<'_>) -> Result<()>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AppliedMigration {
    version: i64,
    name: String,
}

const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    name: "0001_initial_schema",
    apply: apply_initial_schema,
}];

pub(crate) fn migrate(conn: &mut Connection) -> Result<()> {
    run_migrations(conn, MIGRATIONS)?;
    validate_current_schema(conn)
}

fn run_migrations(conn: &mut Connection, available: &[Migration]) -> Result<()> {
    validate_available(available)?;
    let tables = user_tables(conn)?;
    let has_migration_table = tables.iter().any(|table| table == "schema_migrations");

    let applied = if has_migration_table {
        load_applied(conn).context("failed to read database migration history")?
    } else {
        if !tables.is_empty() {
            bail!(
                "database is non-empty but has no migration history; remove the pre-release database and restart"
            );
        }
        Vec::new()
    };

    if has_migration_table && applied.is_empty() {
        bail!(
            "database has an empty migration history; remove the pre-release database and restart"
        );
    }

    validate_applied(&applied, available)?;
    for migration in available.iter().skip(applied.len()) {
        run_one(conn, *migration)?;
    }
    Ok(())
}

fn run_one(conn: &mut Connection, migration: Migration) -> Result<()> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .with_context(|| format!("failed to begin migration {}", migration.name))?;
    (migration.apply)(&tx)
        .with_context(|| format!("failed to apply migration {}", migration.name))?;
    tx.execute(
        "INSERT INTO schema_migrations(version, name, applied_at_unix_ms) VALUES (?1, ?2, ?3)",
        params![
            migration.version,
            migration.name,
            unix_timestamp_ms().unwrap_or_default()
        ],
    )
    .with_context(|| format!("failed to record migration {}", migration.name))?;
    tx.commit()
        .with_context(|| format!("failed to commit migration {}", migration.name))?;
    Ok(())
}

fn load_applied(conn: &Connection) -> Result<Vec<AppliedMigration>> {
    let mut stmt = conn.prepare("SELECT version, name FROM schema_migrations ORDER BY version")?;
    let rows = stmt.query_map([], |row| {
        Ok(AppliedMigration {
            version: row.get(0)?,
            name: row.get(1)?,
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn validate_applied(applied: &[AppliedMigration], available: &[Migration]) -> Result<()> {
    let latest = available
        .last()
        .map(|migration| migration.version)
        .unwrap_or(0);
    for (index, record) in applied.iter().enumerate() {
        let expected_version = i64::try_from(index + 1).context("migration index overflow")?;
        if record.version > latest {
            bail!(
                "database migration version {} is newer than supported version {latest}",
                record.version
            );
        }
        if record.version != expected_version {
            bail!(
                "database migration history has a gap: expected version {expected_version}, found {}",
                record.version
            );
        }
        let expected = available
            .iter()
            .find(|migration| migration.version == record.version)
            .with_context(|| format!("migration version {} is not supported", record.version))?;
        if record.name != expected.name {
            bail!(
                "database migration {} name mismatch: stored `{}`, expected `{}`",
                record.version,
                record.name,
                expected.name
            );
        }
    }
    Ok(())
}

fn validate_available(available: &[Migration]) -> Result<()> {
    for (index, migration) in available.iter().enumerate() {
        let expected_version = i64::try_from(index + 1).context("migration index overflow")?;
        if migration.version != expected_version {
            bail!(
                "migration registry has a gap: expected version {expected_version}, found {}",
                migration.version
            );
        }
        if migration.name.trim().is_empty() {
            bail!("migration {expected_version} has an empty name");
        }
    }
    Ok(())
}

fn user_tables(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn validate_current_schema(conn: &Connection) -> Result<()> {
    let required = [
        "catalog_cache_state",
        "engine_state",
        "provider_cities",
        "provider_provinces",
        "provider_station_mappings",
        "schema_migrations",
        "upstream_fetch_log",
        "weather_snapshots_history",
    ];
    let tables = user_tables(conn)?;
    for table in required {
        if !tables.iter().any(|candidate| candidate == table) {
            bail!("database schema version {LATEST_DB_VERSION} is missing table `{table}`");
        }
    }
    Ok(())
}

fn apply_initial_schema(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        r#"
        CREATE TABLE schema_migrations(
            version INTEGER PRIMARY KEY,
            name TEXT NOT NULL UNIQUE,
            applied_at_unix_ms INTEGER NOT NULL
        );

        CREATE TABLE provider_provinces(
            provider TEXT NOT NULL,
            provider_code TEXT NOT NULL,
            name TEXT NOT NULL,
            url TEXT NOT NULL,
            updated_at_unix_ms INTEGER NOT NULL,
            PRIMARY KEY(provider, provider_code)
        );
        CREATE INDEX idx_provider_provinces_name
            ON provider_provinces(provider, name);

        CREATE TABLE provider_cities(
            provider TEXT NOT NULL,
            provider_code TEXT NOT NULL,
            provider_province_code TEXT NOT NULL,
            province TEXT NOT NULL,
            city TEXT NOT NULL,
            url TEXT NOT NULL,
            updated_at_unix_ms INTEGER NOT NULL,
            PRIMARY KEY(provider, provider_code)
        );
        CREATE INDEX idx_provider_cities_province
            ON provider_cities(provider, provider_province_code, city);

        CREATE TABLE catalog_cache_state(
            provider TEXT NOT NULL,
            catalog_kind TEXT NOT NULL CHECK(catalog_kind IN ('provinces', 'cities')),
            scope TEXT NOT NULL,
            fetched_at_unix_ms INTEGER NOT NULL,
            row_count INTEGER NOT NULL CHECK(row_count >= 0),
            PRIMARY KEY(provider, catalog_kind, scope)
        );

        CREATE TABLE provider_station_mappings(
            provider TEXT NOT NULL,
            display_name TEXT NOT NULL,
            provider_station_id TEXT NOT NULL,
            provider_province_code TEXT NOT NULL,
            province TEXT NOT NULL,
            city TEXT NOT NULL,
            url TEXT NOT NULL,
            name TEXT NOT NULL,
            unified_uuid TEXT NOT NULL CHECK(length(trim(unified_uuid)) > 0),
            updated_at_unix_ms INTEGER NOT NULL,
            PRIMARY KEY(provider, display_name)
        );
        CREATE INDEX idx_provider_station_mappings_uuid
            ON provider_station_mappings(provider, unified_uuid);

        CREATE TABLE upstream_fetch_log(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            unified_uuid TEXT,
            endpoint TEXT NOT NULL,
            ok INTEGER NOT NULL,
            message TEXT,
            fetched_at_unix_ms INTEGER NOT NULL
        );
        CREATE INDEX idx_fetch_log_fetched
            ON upstream_fetch_log(fetched_at_unix_ms DESC, id DESC);

        CREATE TABLE engine_state(
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL,
            updated_at_unix_ms INTEGER NOT NULL
        );
        "#,
    )?;
    create_history_table(tx)?;
    create_history_index(tx)?;
    Ok(())
}

pub(crate) fn create_history_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(HISTORY_TABLE_SQL)
        .context("failed to create canonical weather history table")
}

pub(crate) fn create_history_index(conn: &Connection) -> Result<()> {
    conn.execute_batch(HISTORY_INDEX_SQL)
        .context("failed to create canonical weather history index")
}

/// A timezone rebuild deliberately recreates the canonical history table.  Refuse
/// schema drift and external objects up front so a future migration cannot have
/// columns, indexes, triggers, or views silently discarded.  A future schema
/// migration must update these shared definitions and the SQL copy together.
pub(crate) fn validate_history_rebuild_schema(conn: &Connection) -> Result<()> {
    let table_sql = conn
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = ?1",
            params![HISTORY_TABLE_NAME],
            |row| row.get::<_, String>(0),
        )
        .context("canonical weather history table is missing")?;
    if table_sql != HISTORY_TABLE_SQL {
        bail!(
            "weather history table schema is not supported by timezone migration; run a compatible database migration first"
        );
    }

    let index_sql = conn
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE type = 'index' AND name = ?1 AND tbl_name = ?2",
            params![HISTORY_INDEX_NAME, HISTORY_TABLE_NAME],
            |row| row.get::<_, String>(0),
        )
        .context("canonical weather history index is missing")?;
    if index_sql != HISTORY_INDEX_SQL {
        bail!(
            "weather history index schema is not supported by timezone migration; run a compatible database migration first"
        );
    }

    let mut stmt = conn.prepare(
        r#"SELECT type, name
           FROM sqlite_schema
           WHERE sql IS NOT NULL
             AND NOT (type = 'table' AND name = ?1)
             AND NOT (type = 'index' AND name = ?2 AND tbl_name = ?1)
             AND instr(lower(sql), lower(?3)) > 0
           ORDER BY type, name"#,
    )?;
    let objects = stmt
        .query_map(
            params![HISTORY_TABLE_NAME, HISTORY_INDEX_NAME, HISTORY_TABLE_NAME],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if !objects.is_empty() {
        let objects = objects
            .into_iter()
            .map(|(kind, name)| format!("{kind} `{name}`"))
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "timezone migration does not support extra schema objects referencing `{HISTORY_TABLE_NAME}`: {objects}"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_op(_: &Transaction<'_>) -> Result<()> {
        Ok(())
    }

    fn fail_after_write(tx: &Transaction<'_>) -> Result<()> {
        tx.execute("CREATE TABLE partial_write(id INTEGER PRIMARY KEY)", [])?;
        bail!("injected migration failure")
    }

    fn apply_v2(tx: &Transaction<'_>) -> Result<()> {
        tx.execute("CREATE TABLE v2_data(id INTEGER PRIMARY KEY)", [])?;
        Ok(())
    }

    fn fail_v2_after_write(tx: &Transaction<'_>) -> Result<()> {
        tx.execute("CREATE TABLE v2_partial(id INTEGER PRIMARY KEY)", [])?;
        bail!("injected v2 migration failure")
    }

    #[test]
    fn fresh_database_applies_v1_and_reopens() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        migrate(&mut conn).unwrap();

        let applied = load_applied(&conn).unwrap();
        assert_eq!(
            applied,
            vec![AppliedMigration {
                version: 1,
                name: "0001_initial_schema".to_string(),
            }]
        );
        validate_current_schema(&conn).unwrap();
    }

    #[test]
    fn rejects_non_empty_database_without_migration_history() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute("CREATE TABLE legacy(id INTEGER PRIMARY KEY)", [])
            .unwrap();

        let err = migrate(&mut conn).unwrap_err().to_string();
        assert!(err.contains("non-empty"), "{err}");
    }

    #[test]
    fn rejects_gap_name_mismatch_and_future_version() {
        let available = [
            Migration {
                version: 1,
                name: "one",
                apply: no_op,
            },
            Migration {
                version: 2,
                name: "two",
                apply: no_op,
            },
            Migration {
                version: 3,
                name: "three",
                apply: no_op,
            },
        ];
        let gap = vec![
            AppliedMigration {
                version: 1,
                name: "one".to_string(),
            },
            AppliedMigration {
                version: 3,
                name: "three".to_string(),
            },
        ];
        assert!(
            validate_applied(&gap, &available)
                .unwrap_err()
                .to_string()
                .contains("gap")
        );

        let mismatch = vec![AppliedMigration {
            version: 1,
            name: "wrong".to_string(),
        }];
        assert!(
            validate_applied(&mismatch, &available)
                .unwrap_err()
                .to_string()
                .contains("name mismatch")
        );

        let future = vec![
            AppliedMigration {
                version: 1,
                name: "one".to_string(),
            },
            AppliedMigration {
                version: 2,
                name: "two".to_string(),
            },
            AppliedMigration {
                version: 3,
                name: "three".to_string(),
            },
            AppliedMigration {
                version: 4,
                name: "four".to_string(),
            },
        ];
        assert!(
            validate_applied(&future, &available)
                .unwrap_err()
                .to_string()
                .contains("newer")
        );
    }

    #[test]
    fn failed_migration_rolls_back_schema_and_version() {
        let mut conn = Connection::open_in_memory().unwrap();
        let migration = Migration {
            version: 1,
            name: "failing",
            apply: fail_after_write,
        };

        assert!(run_one(&mut conn, migration).is_err());
        let tables = user_tables(&conn).unwrap();
        assert!(!tables.iter().any(|table| table == "partial_write"));
        assert!(!tables.iter().any(|table| table == "schema_migrations"));
    }

    #[test]
    fn appended_v2_runs_after_v1_and_is_recorded() {
        let migrations = [
            Migration {
                version: 1,
                name: "0001_initial_schema",
                apply: apply_initial_schema,
            },
            Migration {
                version: 2,
                name: "0002_test_data",
                apply: apply_v2,
            },
        ];
        let mut conn = Connection::open_in_memory().unwrap();

        run_migrations(&mut conn, &migrations[..1]).unwrap();
        run_migrations(&mut conn, &migrations).unwrap();

        assert_eq!(
            load_applied(&conn).unwrap(),
            vec![
                AppliedMigration {
                    version: 1,
                    name: "0001_initial_schema".to_string(),
                },
                AppliedMigration {
                    version: 2,
                    name: "0002_test_data".to_string(),
                },
            ]
        );
        assert!(
            user_tables(&conn)
                .unwrap()
                .iter()
                .any(|table| table == "v2_data")
        );
    }

    #[test]
    fn appended_v2_failure_rolls_back_schema_and_history() {
        let v1 = Migration {
            version: 1,
            name: "0001_initial_schema",
            apply: apply_initial_schema,
        };
        let mut conn = Connection::open_in_memory().unwrap();
        run_migrations(&mut conn, &[v1]).unwrap();

        let failing = [
            v1,
            Migration {
                version: 2,
                name: "0002_failing",
                apply: fail_v2_after_write,
            },
        ];
        assert!(run_migrations(&mut conn, &failing).is_err());

        assert_eq!(
            load_applied(&conn).unwrap(),
            vec![AppliedMigration {
                version: 1,
                name: "0001_initial_schema".to_string(),
            }]
        );
        assert!(
            !user_tables(&conn)
                .unwrap()
                .iter()
                .any(|table| table == "v2_partial")
        );
    }
}
