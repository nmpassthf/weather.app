//! Atomically coordinate database timezone migration with persisted/live config.

use std::str::FromStr;

use anyhow::{Context, Result};
use weather_configure::{prepare_config_atomic, validate};
use weather_schema::*;

use crate::runtime::{Engine, EngineExit};

pub(crate) struct TimezoneMigration {
    pub(crate) old_timezone: String,
    pub(crate) new_timezone: String,
    pub(crate) rows_rewritten: u64,
}

impl Engine {
    pub(crate) async fn handle_migrate_db_timezone(&self, request: &RpcRequest) -> RpcResponse {
        let decoded = decode_message::<MigrateDbTimezoneRequest>(&request.payload);
        let Ok(req) = decoded else {
            return Self::rpc_error_response(
                &request.request_id,
                RpcErrorCode::BadRequest,
                decoded.unwrap_err().to_string(),
            );
        };
        if chrono_tz::Tz::from_str(&req.new_timezone).is_err() {
            log::warn!(
                "database timezone migration rejected invalid_timezone={}",
                req.new_timezone
            );
            return Self::rpc_error_response(
                &request.request_id,
                RpcErrorCode::BadRequest,
                format!("invalid timezone `{}`", req.new_timezone),
            );
        }

        match self.migrate_db_timezone(req.new_timezone).await {
            Ok(migration) => {
                log::info!(
                    "database timezone migration completed old_timezone={} new_timezone={} rows_rewritten={}",
                    migration.old_timezone,
                    migration.new_timezone,
                    migration.rows_rewritten
                );
                self.ok(
                    &request.request_id,
                    MigrateDbTimezoneResponse {
                        old_timezone: migration.old_timezone,
                        new_timezone: migration.new_timezone,
                        rows_rewritten: migration.rows_rewritten,
                    },
                )
            }
            Err(err) => {
                log::error!("database timezone migration failed: {err:#}");
                Self::rpc_error_response(
                    &request.request_id,
                    RpcErrorCode::Database,
                    format!("{err:#}"),
                )
            }
        }
    }

    pub(crate) async fn migrate_db_timezone(
        &self,
        new_timezone: String,
    ) -> Result<TimezoneMigration> {
        let commit_guard = self.config_commit.clone().lock_owned().await;
        let current = self.config.get();
        let old_timezone = current.db.timezone.clone();
        log::info!(
            "database timezone migration started old_timezone={} new_timezone={}",
            old_timezone,
            new_timezone
        );

        if old_timezone == new_timezone {
            log::debug!("database timezone migration is an idempotent request");
            let rows_rewritten = self
                .db
                .migrate_timezone_bundle(
                    old_timezone.clone(),
                    new_timezone.clone(),
                    || Ok(()),
                    postcommit_failure(commit_guard, self.config.clone(), self.control.clone()),
                )
                .await?;
            return Ok(TimezoneMigration {
                old_timezone,
                new_timezone,
                rows_rewritten,
            });
        }

        let mut candidate = current;
        candidate.db.timezone = new_timezone.clone();
        validate(&candidate).context("invalid timezone migration config")?;
        let prepare_path = self.config_path.clone();
        let prepare_candidate = candidate.clone();
        let prepared = tokio::task::spawn_blocking(move || {
            prepare_config_atomic(&prepare_path, &prepare_candidate)
        })
        .await
        .context("timezone config prepare task failed")??;

        let finalize_state = self.config.clone();
        let finalize_candidate = candidate;
        let rows_rewritten = self
            .db
            .migrate_timezone_bundle(
                old_timezone.clone(),
                new_timezone.clone(),
                move || {
                    prepared.persist()?;
                    finalize_state.apply(finalize_candidate);
                    Ok(())
                },
                postcommit_failure(commit_guard, self.config.clone(), self.control.clone()),
            )
            .await?;
        Ok(TimezoneMigration {
            old_timezone,
            new_timezone,
            rows_rewritten,
        })
    }
}

fn postcommit_failure(
    commit_guard: tokio::sync::OwnedMutexGuard<()>,
    config: weather_configure::ConfigState,
    control: crate::lifecycle::EngineControl,
) -> impl FnOnce(String) + Send + Sync + 'static {
    move |message| {
        let _commit_guard = commit_guard;
        log::error!(
            "database timezone migration post-commit failed; requesting engine restart: {message}"
        );
        control.request_exit(EngineExit::Restart);
        config.record_error(message);
    }
}
