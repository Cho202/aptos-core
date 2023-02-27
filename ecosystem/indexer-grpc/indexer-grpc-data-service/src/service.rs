// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use aptos_indexer_grpc_utils::{
    build_protobuf_encoded_transaction_wrappers,
    cache_operator::{CacheBatchGetStatus, CacheOperator},
    config::IndexerGrpcConfig,
    file_store_operator::FileStoreOperator,
    EncodedTransactionWithVersion,
};
use aptos_logger::{error, info, warn};
use aptos_moving_average::MovingAverage;
use aptos_protos::datastream::v1::{
    indexer_stream_server::IndexerStream,
    raw_datastream_response::Response as DatastreamProtoResponse, RawDatastreamRequest,
    RawDatastreamResponse, TransactionOutput, TransactionsOutput,
};
use futures::Stream;
use std::{pin::Pin, sync::Arc, time::Duration};
use tokio::sync::mpsc::{channel, error::TrySendError};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use uuid::Uuid;

type ResponseStream = Pin<Box<dyn Stream<Item = Result<RawDatastreamResponse, Status>> + Send>>;

const MOVING_AVERAGE_WINDOW_SIZE: u64 = 10_000;
const AHEAD_OF_CACHE_RETRY_SLEEP_DURATION_MS: u64 = 200;
const TRANSIENT_DATA_ERROR_RETRY_SLEEP_DURATION_MS: u64 = 1000;

// TODO(larry): replace this with a exponential backoff.
// The server will not fetch more data from the cache and file store until the channel is not full.
const RESPONSE_CHANNEL_FULL_BACKOFF_DURATION_MS: u64 = 1000;
// Up to MAX_RESPONSE_CHANNEL_SIZE response can be buffered in the channel. If the channel is full,
// the server will not fetch more data from the cache and file store until the channel is not full.
const MAX_RESPONSE_CHANNEL_SIZE: usize = 40;

pub struct DatastreamServer {
    pub redis_client: Arc<redis::Client>,
    pub server_config: IndexerGrpcConfig,
}

impl DatastreamServer {
    pub fn new(config: IndexerGrpcConfig) -> Self {
        Self {
            redis_client: Arc::new(
                redis::Client::open(format!("redis://{}", config.redis_address))
                    .expect("Create redis client failed."),
            ),
            server_config: config,
        }
    }
}

/// Enum to represent the status of the data fetching.
enum TransactionsDataStatus {
    /// Data fetching is successful.
    Success(Vec<EncodedTransactionWithVersion>),
    /// Ahead of current head of cache.
    AheadOfCache,
    // Fatal error when gap detected between cache and file store.
    DataGap,
}

// DatastreamServer handles the raw datastream requests from cache and file store.
#[tonic::async_trait]
impl IndexerStream for DatastreamServer {
    type RawDatastreamStream = ResponseStream;

    async fn raw_datastream(
        &self,
        req: Request<RawDatastreamRequest>,
    ) -> Result<Response<Self::RawDatastreamStream>, Status> {
        // Response channel to stream the data to the client.
        let (tx, rx) = channel(MAX_RESPONSE_CHANNEL_SIZE);
        let req = req.into_inner();
        let mut current_version = req.starting_version;

        let file_store_bucket_name = self.server_config.file_store_bucket_name.clone();
        let redis_client = self.redis_client.clone();

        tokio::spawn(async move {
            let conn = redis_client.get_async_connection().await.unwrap();
            let mut cache_operator = CacheOperator::new(conn);
            let file_store_operator = FileStoreOperator::new(file_store_bucket_name);
            file_store_operator.bootstrap().await;

            let chain_id = cache_operator.get_chain_id().await.unwrap();
            // Data service metrics.
            let mut tps_calculator = MovingAverage::new(MOVING_AVERAGE_WINDOW_SIZE);
            // Request metadata.
            let request_id = Uuid::new_v4().to_string();

            loop {
                // 1. Fetch data from cache and file store.
                let transaction_data_status =
                    match data_fetch(current_version, &mut cache_operator, &file_store_operator)
                        .await
                    {
                        Ok(status) => status,
                        Err(e) => {
                            error!(
                            request_id = request_id.as_str(),
                            current_version = current_version,
                            "[Indexer Data] Failed to fetch data from cache and file store. {:?}",
                            e
                        );
                            // Treat all file&cache related errors transient and retry.
                            tokio::time::sleep(Duration::from_millis(
                                TRANSIENT_DATA_ERROR_RETRY_SLEEP_DURATION_MS,
                            ))
                            .await;
                            continue;
                        },
                    };

                // 2.Branch based on the data fetching status.
                let transaction_data: Vec<EncodedTransactionWithVersion> =
                    match transaction_data_status {
                        TransactionsDataStatus::Success(data) => data,
                        TransactionsDataStatus::AheadOfCache => {
                            tokio::time::sleep(Duration::from_millis(
                                AHEAD_OF_CACHE_RETRY_SLEEP_DURATION_MS,
                            ))
                            .await;
                            continue;
                        },
                        TransactionsDataStatus::DataGap => {
                            // TODO(larry): add metrics/alerts to track the gap.
                            // Do not crash the server when gap detected since other clients may still be able to get data.
                            break;
                        },
                    };

                // 3. Push the data to the response channel, i.e. stream the data to the client.
                let current_batch_size = transaction_data.len();
                let resp_item = raw_datastream_response_builder(transaction_data, chain_id as u32);
                match tx.try_send(Result::<RawDatastreamResponse, Status>::Ok(resp_item)) {
                    Ok(_) => {},
                    Err(TrySendError::Full(_)) => {
                        warn!(
                            request_id = request_id.as_str(),
                            "[Indexer Data] Receiver is full; retrying."
                        );
                        tokio::time::sleep(Duration::from_millis(
                            RESPONSE_CHANNEL_FULL_BACKOFF_DURATION_MS,
                        ))
                        .await;
                        continue;
                    },
                    Err(TrySendError::Closed(_)) => {
                        warn!(
                            request_id = request_id.as_str(),
                            "[Indexer Data] Receiver is closed; exiting."
                        );
                        break;
                    },
                }
                // 4. Update the current version and record current tps.
                tps_calculator.tick_now(current_batch_size as u64);
                current_version += current_batch_size as u64;
                info!(
                    request_id = request_id.as_str(),
                    current_version = current_version,
                    batch_size = current_batch_size,
                    tps = (tps_calculator.avg() * 1000.0) as u64,
                    "[Indexer Data] Sending batch."
                );
            }
            info!(
                request_id = request_id.as_str(),
                "[Indexer Data] Client disconnected."
            );
        });

        let output_stream = ReceiverStream::new(rx);
        Ok(Response::new(
            Box::pin(output_stream) as Self::RawDatastreamStream
        ))
    }
}

/// Builds the response for the raw datastream request. Partial batch is ok, i.e., a batch with transactions < 1000.
fn raw_datastream_response_builder(
    data: Vec<EncodedTransactionWithVersion>,
    chain_id: u32,
) -> RawDatastreamResponse {
    RawDatastreamResponse {
        response: Some(DatastreamProtoResponse::Data(TransactionsOutput {
            transactions: data
                .into_iter()
                .map(|(encoded, version)| TransactionOutput {
                    encoded_proto_data: encoded,
                    version,
                    ..TransactionOutput::default()
                })
                .collect(),
        })),
        chain_id,
    }
}

/// Fetches data from cache or the file store. It returns the data if it is ready in the cache or file store.
/// Otherwise, it returns the status of the data fetching.
async fn data_fetch(
    starting_version: u64,
    cache_operator: &mut CacheOperator<redis::aio::Connection>,
    file_store_operator: &FileStoreOperator,
) -> anyhow::Result<TransactionsDataStatus> {
    let batch_get_result = cache_operator
        .batch_get_encoded_proto_data(starting_version)
        .await;

    match batch_get_result {
        // Data is not ready yet in the cache.
        Ok(CacheBatchGetStatus::NotReady) => Ok(TransactionsDataStatus::AheadOfCache),
        Ok(CacheBatchGetStatus::Ok(v)) => Ok(TransactionsDataStatus::Success(
            build_protobuf_encoded_transaction_wrappers(v, starting_version),
        )),
        Ok(CacheBatchGetStatus::EvictedFromCache) => {
            // Data is evicted from the cache. Fetch from file store.
            let file_store_batch_get_result =
                file_store_operator.get_transactions(starting_version).await;
            match file_store_batch_get_result {
                Ok(v) => Ok(TransactionsDataStatus::Success(
                    build_protobuf_encoded_transaction_wrappers(v, starting_version),
                )),
                Err(e) => {
                    if e.to_string().contains("Transactions file not found") {
                        Ok(TransactionsDataStatus::DataGap)
                    } else {
                        Err(e)
                    }
                },
            }
        },
        Err(e) => Err(e),
    }
}
