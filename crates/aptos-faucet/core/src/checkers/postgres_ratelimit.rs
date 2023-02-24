// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use super::{Checker, CheckerData, CompleteData};
use crate::{
    endpoints::{AptosTapError, AptosTapErrorCode, RejectionReason, RejectionReasonCode},
    helpers::get_current_time_secs,
};
use anyhow::{Context, Result};
use aptos_faucet_migration::{Migrator, MigratorTrait};
use aptos_logger::info;
use async_trait::async_trait;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectOptions, Database, DatabaseConnection, EntityTrait,
    QueryFilter, QuerySelect, Set, Unchanged,
};
use serde::{Deserialize, Serialize};
use std::time::Duration;

// It's not great that we're encoding some checking logic here in the storage
// layer, but the alternative is adding functions like `start_transaction`
// `end_transaction` to the storage trait with some kind of generic transaction
// or lock object, which is pretty heavy for this use case. If the pre-insertion
// checking logic becomes too heavy we can reconsider something like this.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PostgresRatelimitCheckerConfig {
    /// The database address to connect to, not including port, e.g. db.example.com or 234.121.222.42.
    pub database_address: String,

    /// The port to connect to.
    #[serde(default = "PostgresRatelimitCheckerConfig::default_database_port")]
    pub database_port: u16,

    /// The name of the database to use. If it doesn't exist, it will be created.
    pub database_name: String,

    /// The name of the user to use.
    pub database_user: String,

    /// The password of the given user, if necessary.
    pub database_password: Option<String>,

    /// Max number of successful requests per IP.
    pub max_requests_per_ip: u64,

    /// Do not run migrations on startup.
    pub do_not_run_migrations: bool,

    /// If a value is given, rows older than this number of seconds from now
    /// will be removed from the DB. In combination with `--max-requests-per-ip`,
    /// this allows you to phrase something like `--max-requests-per-ip-per-day`.
    pub row_ttl_secs: Option<u64>,

    /// How often to run the DB reaper task if enabled.
    #[serde(default = "PostgresRatelimitCheckerConfig::default_db_reaper_task_interval_secs")]
    pub db_reaper_task_interval_secs: u64,
}

impl PostgresRatelimitCheckerConfig {
    fn default_database_port() -> u16 {
        5432
    }

    fn default_db_reaper_task_interval_secs() -> u64 {
        300
    }

    fn build_database_url(&self) -> String {
        format!(
            "postgres://{}:{}@{}:{}/{}",
            self.database_user,
            self.database_password.as_deref().unwrap_or(""),
            self.database_address,
            self.database_port,
            self.database_name
        )
    }

    pub async fn build_database_connection(&self) -> Result<DatabaseConnection> {
        let mut opt = ConnectOptions::new(self.build_database_url());
        opt.max_connections(64)
            .min_connections(8)
            .connect_timeout(Duration::from_secs(6))
            .idle_timeout(Duration::from_secs(6))
            .max_lifetime(Duration::from_secs(6))
            .sqlx_logging(true);
        Database::connect(opt)
            .await
            .context("Failed to connect to database")
    }
}

pub struct PostgresRatelimitChecker {
    args: PostgresRatelimitCheckerConfig,
    db: DatabaseConnection,
}

impl PostgresRatelimitChecker {
    pub async fn new(args: PostgresRatelimitCheckerConfig) -> Result<Self> {
        let db = args.build_database_connection().await?;

        // Run DB migrations if necessary.
        if !args.do_not_run_migrations {
            Migrator::up(&db, None)
                .await
                .context("Failed to run DB migrations")?;
            info!("Ran DB migrations (if necessary) on startup successfully");
        } else {
            info!("Skipping DB migrations as requested");
        }

        Ok(Self { args, db })
    }

    /// This function finds rows that have been sitting in the DB for more than
    /// `row_ttl_secs` seconds and deletes them. If this fails more than n times,
    /// the function returns, which will ultimately cause the process to exit.
    async fn clear_old_rows(args: PostgresRatelimitCheckerConfig, row_ttl_secs: u64) -> Result<()> {
        let db = args.build_database_connection().await?;
        let mut error_count = 0;
        loop {
            let current_time = get_current_time_secs();
            let cutoff_time = current_time - row_ttl_secs;

            let result = aptos_faucet_entity::request::Entity::delete_many()
                .filter(
                    aptos_faucet_entity::request::Column::InsertionUnixtimeSecs
                        .lt(cutoff_time as i64),
                )
                .exec(&db)
                .await
                .context("Failed to delete old rows");

            match result {
                Ok(result) => {
                    info!(
                        "[OldRowReaper]: Reaped {} old rows from the DB",
                        result.rows_affected
                    );
                    error_count = 0;
                },
                Err(e) => {
                    error_count += 1;
                    if error_count > 5 {
                        return Err(e).context("Failed to delete old rows too many times");
                    }
                    info!(
                        "[OldRowReaper]: Failed to delete old rows from the DB (this has happend {} times now), will retry later",
                        error_count
                    );
                },
            }

            tokio::time::sleep(std::time::Duration::from_secs(
                args.db_reaper_task_interval_secs,
            ))
            .await;
        }
    }
}

#[async_trait]
impl Checker for PostgresRatelimitChecker {
    /// This function is responsible for checking that the given request is
    /// valid based on a number of criteria, e.g. how many successful / ongoing
    /// requests have come from that IP, and then if everything looks good,
    /// inserting the request into the database.
    ///
    /// This method is not intended to completely prevent abuse of the faucet;
    /// there is a small window between where we read the rows for the given IP
    /// and insert new rows that could be exploited if someone were to send many
    /// requests for a single IP all at the same time. Realistically this isn't
    /// exploitable because the key for a row includes the insertion unixtime in
    /// seconds, meaning there can only be one row per second per IP, in which
    /// time this window will have closed unless storage is being really slow.
    ///
    /// The case where we accidentally double fund a given account is not handled
    /// by storage either. Instead, the Funder users a Move script that atomically
    /// asserts that the given account doesn't exist yet and then funds it.
    ///
    /// This function returns insertion_unixtime_secs.
    async fn check(
        &self,
        data: CheckerData,
        dry_run: bool,
    ) -> Result<Vec<RejectionReason>, AptosTapError> {
        // Find all rows for the source IP where the request was either completed
        // successfully or is ongoing. We don't ratelimit unsuccessful requests,
        // we leave that to an LB in front of the service.
        let rows: Vec<aptos_faucet_entity::request::Model> =
            aptos_faucet_entity::request::Entity::find()
                .filter(aptos_faucet_entity::request::Column::Ip.eq(data.source_ip.to_string()))
                .filter(aptos_faucet_entity::request::Column::CompletedUnixtimeSecs.gt(0))
                .limit(self.args.max_requests_per_ip)
                .all(&self.db)
                .await
                .map_err(|e| {
                    AptosTapError::new_with_error_code(e, AptosTapErrorCode::StorageError)
                })?;

        // If the source IP is at the limit, reject this new request.
        if rows.len() >= self.args.max_requests_per_ip as usize {
            return Ok(vec![RejectionReason::new(
                format!(
                    "IP {} has reached the maximum number of requests: {}",
                    data.source_ip, self.args.max_requests_per_ip
                ),
                RejectionReasonCode::IpUsageLimitExhausted,
            )]);
        }

        // At this point we've determined this is a valid request, insert the row.
        if !dry_run {
            let amount = i64::try_from(data.amount).map_err(|e| {
                AptosTapError::new_with_error_code(e, AptosTapErrorCode::InvalidRequest)
            })?;
            let model = aptos_faucet_entity::request::ActiveModel {
                ip: Set(data.source_ip.to_string()),
                account_address: Set(data.receiver.to_hex()),
                amount: Set(amount),
                insertion_unixtime_secs: Set(data.time_request_received_secs as i64),
                ..Default::default()
            };

            model.insert(&self.db).await.map_err(|e| {
                AptosTapError::new_with_error_code(
                    format!("Failed to insert request: {}", e),
                    AptosTapErrorCode::StorageError,
                )
            })?;
        };

        Ok(vec![])
    }

    async fn complete(&self, data: CompleteData) -> Result<(), AptosTapError> {
        // Determine whether to mark this as a success.
        let completed_unixtime_secs = match data.response_is_500 {
            true => 0,
            false => get_current_time_secs() as i64,
        };

        let new_txn_hashes = data
            .txn_hashes
            .iter()
            .map(|h| h.to_string())
            .collect::<Vec<String>>()
            .join(",");

        // Update the row. We define the rows used as the "select" for the update
        // by using Unchanged, indiciating we're looking for these fields but we're
        // not changing them. We then use Set to indicate the fields that we do
        // want to change.
        sea_orm::Update::one(aptos_faucet_entity::request::ActiveModel {
            ip: Unchanged(data.checker_data.source_ip.to_string()),
            account_address: Unchanged(data.checker_data.receiver.to_hex()),
            insertion_unixtime_secs: Unchanged(data.checker_data.time_request_received_secs as i64),
            txn_hashes: Set(Some(new_txn_hashes)),
            completed_unixtime_secs: Set(Some(completed_unixtime_secs)),
            ..Default::default()
        })
        .exec(&self.db)
        .await
        .map_err(|e| {
            AptosTapError::new_with_error_code(
                format!("Failed to mark request as complete: {}", e),
                AptosTapErrorCode::StorageError,
            )
        })?;

        Ok(())
    }

    fn spawn_periodic_tasks(&self, join_set: &mut tokio::task::JoinSet<anyhow::Result<()>>) {
        if let Some(row_ttl_secs) = self.args.row_ttl_secs {
            join_set.spawn(Self::clear_old_rows(self.args.clone(), row_ttl_secs));
        }
    }

    fn cost(&self) -> u8 {
        100
    }
}
