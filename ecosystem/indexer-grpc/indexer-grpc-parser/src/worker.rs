// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use crate::{processors::default_processor::DefaultTransactionProcessor,
    indexer::transaction_processor::TransactionProcessor};
use diesel::{
    pg::PgConnection,
    r2d2::{ConnectionManager, PooledConnection},
    RunQueryDsl,
};
use std::sync::Arc;

pub type PgPool = diesel::r2d2::Pool<ConnectionManager<PgConnection>>;
pub type PgDbPool = Arc<PgPool>;
pub type PgPoolConnection = PooledConnection<ConnectionManager<PgConnection>>;

use aptos_logger::{error, info};
use aptos_moving_average::MovingAverage;
use aptos_protos::{
    datastream::v1::{self as datastream},
    transaction::v1::Transaction as TransactionProto,
};
use futures::StreamExt;
use prost::Message;
use tokio::sync::mpsc::{self, error::TrySendError};

const TRANSACTION_CHANNEL_SIZE: usize = 50_000;
const MAX_TRANSACTION_BATCH_SIZE: usize = 5000;
// Will replace these with yaml config
fn get_datastream_service_address() -> String {
    std::env::var("APTOS_DATASTREAM_SERVICE_ADDRESS_VAR")
        .expect("DATASTREAM_SERVICE_ADDRESS is required.")
}

fn get_postgres_connection_string() -> String {
    std::env::var("APTOS_POSTGRES_CONNECTION_STRING_VAR")
        .expect("POSTGRES_CONNECTION_STRING is required.")
}

fn get_starting_version() -> u64 {
    std::env::var("STARTING_VERSION")
        .expect("STARTING_VERSION is required.")
        .parse::<u64>()
        .unwrap()
}

pub struct Worker {
    pub db_pool: PgDbPool,
    pub datastream_service_address: String,
    pub postgres_uri: String,
}

impl Worker {
    pub async fn new() -> Self {
        let postgres_uri = get_postgres_connection_string();
        let manager = ConnectionManager::<PgConnection>::new(postgres_uri.clone());
        let pg_pool = PgPool::builder().build(manager).map(Arc::new);
        Self {
            db_pool: pg_pool.unwrap(),
            datastream_service_address: get_datastream_service_address(),
            postgres_uri,
        }
    }

    pub async fn run(&self) {
        let (tx, mut rx) = mpsc::channel::<TransactionProto>(TRANSACTION_CHANNEL_SIZE);
        let mut ma = MovingAverage::new(10_000);
        let default_transaction_processor = Arc::new(DefaultTransactionProcessor::new(self.db_pool.clone()));
        // Re-connect if lost.
        tokio::spawn(async move {
            // Nothing speicial.
            let default_transaction_processor :Arc<dyn TransactionProcessor> = default_transaction_processor.clone();
            loop {
                let mut current_transactions = vec![];
                for _ in 0..MAX_TRANSACTION_BATCH_SIZE {
                    let transaction = match rx.recv().await {
                        Some(t) => t,
                        None => {
                            info!("[Datastream Indexer] Channel is empty now.");
                            break;
                        },
                    };
                    current_transactions.push(transaction);
                }
                if current_transactions.is_empty() {
                    continue;
                }
                let starting_version = current_transactions.as_slice().first().unwrap().version;
                let batch_size = current_transactions.len();
                default_transaction_processor.process_transactions_with_status(current_transactions).await.unwrap();
                ma.tick_now(batch_size as u64);
                info!(
                    starting_version = starting_version,
                    batch_size = batch_size,
                    tps = (ma.avg() * 1000.0) as u64,
                    "[Datastream Indexer] Batch inserted.",
                );
            }
        });

        loop {
            let mut rpc_client =
                match datastream::indexer_stream_client::IndexerStreamClient::connect(format!(
                    "http://{}:50052",
                    self.datastream_service_address,
                ))
                .await
                {
                    Ok(client) => client,
                    Err(e) => {
                        error!(
                            "[Datasteram Worker]Error connecting to indexer address {}, postgres uri {}",
                            self.datastream_service_address, self.postgres_uri
                        );
                        panic!("[Datastream Worker] Error connecting to indexer: {}", e);
                    },
                };
            let request = tonic::Request::new(datastream::RawDatastreamRequest {
                // Loads from the recent successful starting version.
                starting_version: get_starting_version(),
            });

            let response = rpc_client.raw_datastream(request).await.unwrap();
            let mut resp_stream = response.into_inner();
            let mut init_signal_received = false;

            while let Some(received) = resp_stream.next().await {
                let received = match received {
                    Ok(r) => r,
                    Err(e) => {
                        // If the connection is lost, reconnect.
                        error!(
                            "[Datastream Worker] Error receiving datastream response: {}",
                            e
                        );
                        break;
                    },
                };
                match received.response.unwrap() {
                    datastream::raw_datastream_response::Response::Status(status) => {
                        match status.r#type {
                            0 => {
                                if init_signal_received {
                                    error!("[Datastream Indexer] No signal is expected; panic.");
                                    panic!("[Datastream Indexer] No signal is expected; panic.");
                                } else {
                                    // The first signal is the initialization signal.
                                    init_signal_received = true;
                                }
                            },
                            1 => {
                                // No BATCH_END signal is expected.
                                error!("[Datastream Indexer] No signal is expected; panic.");
                                panic!("[Datastream Indexer] No signal is expected; panic.");
                            },
                            _ => {
                                // There might be protobuf inconsistency between server and client.
                                // Panic to block running.
                                panic!("[Datastream Worker] Unknown RawDatastreamResponse status type.");
                            },
                        }
                    },
                    datastream::raw_datastream_response::Response::Data(data) => {
                        let transaction_sender = tx.clone();

                        let transactions: Vec<TransactionProto> = data
                            .transactions
                            .into_iter()
                            .map(|e| {
                                let txn_raw = base64::decode(e.encoded_proto_data).unwrap();
                                TransactionProto::decode(&*txn_raw).unwrap()
                            })
                            .collect();

                        for txn in transactions {
                            let mut is_pending_for_sending = true;
                            while is_pending_for_sending {
                                match transaction_sender.try_send(txn.clone()) {
                                    Ok(_) => {
                                        is_pending_for_sending = false;
                                    },
                                    Err(TrySendError::Full(_)) => {
                                        error!("[Datastream Worker] Error sending transaction to channel.");
                                        tokio::time::sleep(std::time::Duration::from_millis(1000))
                                            .await;
                                    },
                                    Err(e) => {
                                        panic!("[Datastream Worker] Error sending transaction to channel: {}", e);
                                    },
                                }
                            }
                        }
                    },
                };
            }
        }
    }
}
