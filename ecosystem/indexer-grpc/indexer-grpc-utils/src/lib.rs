// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

pub mod cache_operator;
pub mod config;
pub mod constants;
pub mod file_store_operator;

use aptos_protos::datastream::v1::indexer_stream_client::IndexerStreamClient;

pub type GrpcClientType = IndexerStreamClient<tonic::transport::Channel>;

/// Create a gRPC client with exponential backoff.
pub async fn create_grpc_client(address: String) -> GrpcClientType {
    backoff::future::retry(backoff::ExponentialBackoff::default(), || async {
        match IndexerStreamClient::connect(address.clone()).await {
            Ok(client) => {
                aptos_logger::info!(
                    address = address.clone(),
                    "[Indexer Cache] Connected to indexer gRPC server."
                );
                Ok(client)
            },
            Err(e) => {
                aptos_logger::error!(
                    address = address.clone(),
                    "[Indexer Cache] Failed to connect to indexer gRPC server: {}",
                    e
                );
                Err(backoff::Error::transient(e))
            },
        }
    })
    .await
    .unwrap()
}
