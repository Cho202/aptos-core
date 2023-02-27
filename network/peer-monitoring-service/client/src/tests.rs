// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use crate::{
    peer_states::key_value::{PeerStateKey, StateValueInterface},
    start_peer_monitor_with_state, PeerMonitorState, PeerMonitoringServiceClient,
};
use aptos_channels::{aptos_channel, message_queues::QueueStyle};
use aptos_config::{
    config::{
        LatencyMonitoringConfig, NetworkMonitoringConfig, NodeConfig, PeerMonitoringServiceConfig,
        PeerRole,
    },
    network_id::{NetworkId, PeerNetworkId},
};
use aptos_netcore::transport::ConnectionOrigin;
use aptos_network::{
    application::{
        interface::NetworkClient,
        metadata::{ConnectionState, PeerMetadata},
        storage::PeersAndMetadata,
    },
    peer_manager::{ConnectionRequestSender, PeerManagerRequest, PeerManagerRequestSender},
    protocols::{
        network::{NetworkSender, NewNetworkSender},
        wire::handshake::v1::ProtocolId,
    },
    transport::ConnectionMetadata,
};
use aptos_peer_monitoring_service_server::network::{NetworkRequest, ResponseSender};
use aptos_peer_monitoring_service_types::{
    LatencyPingResponse, NetworkInformationResponse, PeerMonitoringServiceMessage,
    PeerMonitoringServiceRequest, PeerMonitoringServiceResponse, ServerProtocolVersionResponse,
};
use aptos_time_service::{MockTimeService, TimeService, TimeServiceTrait};
use aptos_types::PeerId;
use futures::StreamExt;
use maplit::hashmap;
use std::{
    cmp::min,
    collections::HashMap,
    future::Future,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    runtime::Handle,
    time::{sleep, timeout},
};

// Useful test constants
const PAUSE_FOR_MONITOR_SETUP_SECS: u64 = 1;
const MAX_WAIT_TIME_SECS: u64 = 10;
const SLEEP_DURATION_MS: u64 = 500;

#[tokio::test(flavor = "multi_thread")]
async fn test_basic_peer_monitor_loop() {
    // Create the peer monitoring client and server
    let network_id = NetworkId::Validator;
    let (peer_monitoring_client, mut mock_monitoring_server, peer_monitor_state, time_service) =
        MockMonitoringServer::new(None, Some(network_id));

    // Spawn the peer monitoring client
    let node_config = NodeConfig::default();
    start_peer_monitor(
        peer_monitoring_client,
        &peer_monitor_state,
        &time_service,
        &node_config,
    )
    .await;

    // Verify the initial state of the peer monitor
    verify_empty_peer_states(&peer_monitor_state);

    // Add a connected validator peer
    let validator_peer =
        mock_monitoring_server.add_peer_with_network_id(network_id, PeerRole::Validator);

    // Initialize all the peer states by running the peer monitor once
    let mock_time = time_service.into_mock();
    let (connected_peers_and_metadata, distance_from_validators) =
        initialize_and_verify_peer_states(
            &mut mock_monitoring_server,
            &peer_monitor_state,
            &node_config,
            &validator_peer,
            &mock_time,
        )
        .await;

    // Elapse enough time for a latency ping and verify correct execution
    verify_and_handle_latency_ping(
        &mut mock_monitoring_server,
        &peer_monitor_state,
        &node_config,
        &validator_peer,
        &mock_time,
        1,
        2,
    )
    .await;

    // Elapse enough time for a network info request (which should also
    // trigger another latency ping).
    let time_before_update = mock_time.now();
    elapse_network_info_update_interval(node_config, mock_time).await;

    // Verify that both a latency and network request are received and respond
    verify_all_requests_and_respond(
        &mut mock_monitoring_server,
        &connected_peers_and_metadata,
        distance_from_validators,
    )
    .await;

    // Wait until the network peer state is updated by the client
    wait_for_peer_state_update(
        time_before_update,
        &peer_monitor_state,
        &validator_peer,
        PeerStateKey::get_all_keys(),
    )
    .await;

    // Verify the new state of the peer monitor
    verify_peer_monitor_state(
        &peer_monitor_state,
        &validator_peer,
        &connected_peers_and_metadata,
        distance_from_validators,
        3,
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_peer_monitor_latency_pings() {
    // Create the peer monitoring client and server
    let network_id = NetworkId::Validator;
    let (peer_monitoring_client, mut mock_monitoring_server, peer_monitor_state, time_service) =
        MockMonitoringServer::new(None, Some(network_id));

    // Create a node config where network info requests don't refresh
    let node_config = get_config_without_network_info_requests();

    // Spawn the peer monitoring client
    start_peer_monitor(
        peer_monitoring_client,
        &peer_monitor_state,
        &time_service,
        &node_config,
    )
    .await;

    // Verify the initial state of the peer monitor
    verify_empty_peer_states(&peer_monitor_state);

    // Add a connected validator peer
    let validator_peer =
        mock_monitoring_server.add_peer_with_network_id(network_id, PeerRole::Validator);

    // Initialize all the peer states by running the peer monitor once
    let mock_time = time_service.into_mock();
    let _ = initialize_and_verify_peer_states(
        &mut mock_monitoring_server,
        &peer_monitor_state,
        &node_config,
        &validator_peer,
        &mock_time,
    )
    .await;

    // Handle many latency ping requests and responses
    let latency_monitoring_config = &node_config.peer_monitoring_service.latency_monitoring;
    let max_num_pings_to_retain = latency_monitoring_config.max_num_latency_pings_to_retain as u64;
    for i in 0..max_num_pings_to_retain * 2 {
        verify_and_handle_latency_ping(
            &mut mock_monitoring_server,
            &peer_monitor_state,
            &node_config,
            &validator_peer,
            &mock_time,
            i + 1,
            min(i + 2, max_num_pings_to_retain), // Only retain max number of pings
        )
        .await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_peer_monitor_latency_pings_failures() {
    // Create the peer monitoring client and server
    let network_id = NetworkId::Validator;
    let (peer_monitoring_client, mut mock_monitoring_server, peer_monitor_state, time_service) =
        MockMonitoringServer::new(None, Some(network_id));

    // Create a node config where network info requests don't refresh
    let node_config = get_config_without_network_info_requests();

    // Spawn the peer monitoring client
    start_peer_monitor(
        peer_monitoring_client,
        &peer_monitor_state,
        &time_service,
        &node_config,
    )
    .await;

    // Add a connected validator peer
    let validator_peer =
        mock_monitoring_server.add_peer_with_network_id(network_id, PeerRole::Validator);

    // Initialize all the peer states by running the peer monitor once
    let mock_time = time_service.into_mock();
    let _ = initialize_and_verify_peer_states(
        &mut mock_monitoring_server,
        &peer_monitor_state,
        &node_config,
        &validator_peer,
        &mock_time,
    )
    .await;

    // Handle several latency ping requests with bad responses
    for i in 0..5 {
        // Elapse enough time for a latency ping update
        elapse_latency_update_interval(node_config.clone(), mock_time.clone()).await;

        // Verify that a single latency request is received and send a bad response
        verify_latency_request_and_respond(&mut mock_monitoring_server, i + 1, false, true, false)
            .await;

        // Wait until the latency peer state is updated with the failure
        wait_for_latency_ping_failure(&peer_monitor_state, &validator_peer, i + 1).await;
    }

    // Handle several latency ping requests with invalid counter responses
    for i in 5..10 {
        // Elapse enough time for a latency ping update
        elapse_latency_update_interval(node_config.clone(), mock_time.clone()).await;

        // Verify that a single latency request is received and send an invalid counter response
        verify_latency_request_and_respond(&mut mock_monitoring_server, i + 1, true, false, false)
            .await;

        // Wait until the latency peer state is updated with the failure
        wait_for_latency_ping_failure(&peer_monitor_state, &validator_peer, i + 1).await;
    }

    // Verify the new latency state of the peer monitor
    verify_peer_latency_state(&peer_monitor_state, &validator_peer, 1, 10);

    // Elapse enough time for a latency ping and perform a successful execution
    verify_and_handle_latency_ping(
        &mut mock_monitoring_server,
        &peer_monitor_state,
        &node_config,
        &validator_peer,
        &mock_time,
        11,
        2,
    )
    .await;

    // Verify the new latency state of the peer monitor (the number
    // of failures should have been reset).
    verify_peer_latency_state(&peer_monitor_state, &validator_peer, 2, 0);

    // Handle several latency ping requests without responses
    for i in 11..16 {
        // Elapse enough time for a latency ping update
        elapse_latency_update_interval(node_config.clone(), mock_time.clone()).await;

        // Verify that a single latency request is received and don't send a response
        verify_latency_request_and_respond(&mut mock_monitoring_server, i + 1, false, false, true)
            .await;

        // Wait until the latency peer state is updated with the failure
        wait_for_latency_ping_failure(&peer_monitor_state, &validator_peer, i - 10).await;
    }

    // Verify the new latency state of the peer monitor
    verify_peer_latency_state(&peer_monitor_state, &validator_peer, 2, 5);

    // Elapse enough time for a latency ping and perform a successful execution
    verify_and_handle_latency_ping(
        &mut mock_monitoring_server,
        &peer_monitor_state,
        &node_config,
        &validator_peer,
        &mock_time,
        17,
        3,
    )
    .await;

    // Verify the new latency state of the peer monitor (the number
    // of failures should have been reset).
    verify_peer_latency_state(&peer_monitor_state, &validator_peer, 3, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_peer_monitor_network_info_requests() {
    // Create the peer monitoring client and server
    let network_id = NetworkId::Validator;
    let (peer_monitoring_client, mut mock_monitoring_server, peer_monitor_state, time_service) =
        MockMonitoringServer::new(None, Some(network_id));

    // Create a node config where latency pings don't refresh
    let node_config = get_config_without_latency_pings();

    // Spawn the peer monitoring client
    start_peer_monitor(
        peer_monitoring_client,
        &peer_monitor_state,
        &time_service,
        &node_config,
    )
    .await;

    // Verify the initial state of the peer monitor
    verify_empty_peer_states(&peer_monitor_state);

    // Add a connected validator peer
    let validator_peer =
        mock_monitoring_server.add_peer_with_network_id(network_id, PeerRole::Validator);

    // Initialize all the peer states by running the peer monitor once
    let mock_time = time_service.into_mock();
    let _ = initialize_and_verify_peer_states(
        &mut mock_monitoring_server,
        &peer_monitor_state,
        &node_config,
        &validator_peer,
        &mock_time,
    )
    .await;

    // Handle several network info requests and responses
    for _ in 0..5 {
        // Elapse enough time for a network info update
        let time_before_update = mock_time.now();
        elapse_network_info_update_interval(node_config.clone(), mock_time.clone()).await;

        // Verify that a single network info request is received and respond
        let connected_peers_and_metadata = hashmap! { PeerNetworkId::random() => PeerMetadata::new(ConnectionMetadata::mock(PeerId::random())) };
        let distance_from_validators = 0;
        verify_network_info_request_and_respond(
            &mut mock_monitoring_server,
            &connected_peers_and_metadata,
            distance_from_validators,
        )
        .await;

        // Wait until the network info state is updated by the client
        wait_for_peer_state_update(
            time_before_update,
            &peer_monitor_state,
            &validator_peer,
            vec![PeerStateKey::NetworkInfo],
        )
        .await;

        // Verify the new network info state of the peer monitor
        verify_peer_network_state(
            &peer_monitor_state,
            &validator_peer,
            &connected_peers_and_metadata,
            distance_from_validators,
        );
    }
}

/// Elapses enough time for a latency update to occur
async fn elapse_latency_update_interval(node_config: NodeConfig, mock_time: MockTimeService) {
    let latency_monitoring_config = node_config.peer_monitoring_service.latency_monitoring;
    mock_time
        .advance_ms_async(latency_monitoring_config.latency_ping_interval_ms + 1)
        .await;
}

/// Elapses enough time for a network info update to occur
async fn elapse_network_info_update_interval(node_config: NodeConfig, mock_time: MockTimeService) {
    let network_monitoring_config = node_config.peer_monitoring_service.network_monitoring;
    mock_time
        .advance_ms_async(network_monitoring_config.network_info_request_interval_ms + 1)
        .await;
}

/// Elapses enough time for the monitoring loop to execute
async fn elapse_peer_monitor_interval(node_config: NodeConfig, mock_time: MockTimeService) {
    let peer_monitoring_config = node_config.peer_monitoring_service;
    mock_time
        .advance_ms_async(peer_monitoring_config.peer_monitor_interval_ms + 1)
        .await;
}

/// Returns a config where latency pings don't refresh
fn get_config_without_latency_pings() -> NodeConfig {
    NodeConfig {
        peer_monitoring_service: PeerMonitoringServiceConfig {
            latency_monitoring: LatencyMonitoringConfig {
                latency_ping_interval_ms: 1_000_000_000, // Unrealistically high
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Returns a config where network info requests don't refresh
fn get_config_without_network_info_requests() -> NodeConfig {
    NodeConfig {
        peer_monitoring_service: PeerMonitoringServiceConfig {
            network_monitoring: NetworkMonitoringConfig {
                network_info_request_interval_ms: 1_000_000_000, // Unrealistically high
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Initializes all the peer states by running the peer monitor loop
/// once and ensuring the correct requests and responses are received.
async fn initialize_and_verify_peer_states(
    mock_monitoring_server: &mut MockMonitoringServer,
    peer_monitor_state: &PeerMonitorState,
    node_config: &NodeConfig,
    validator_peer: &PeerNetworkId,
    mock_time: &MockTimeService,
) -> (HashMap<PeerNetworkId, PeerMetadata>, u64) {
    // Elapse enough time for the peer monitor to execute
    let time_before_update = mock_time.now();
    elapse_peer_monitor_interval(node_config.clone(), mock_time.clone()).await;

    // Verify the initial client requests and send responses
    let connected_peers_and_metadata = hashmap! { PeerNetworkId::random() => PeerMetadata::new(ConnectionMetadata::mock(PeerId::random())) };
    let distance_from_validators = 0;
    verify_all_requests_and_respond(
        mock_monitoring_server,
        &connected_peers_and_metadata,
        distance_from_validators,
    )
    .await;

    // Wait until the peer state is updated by the client
    wait_for_peer_state_update(
        time_before_update,
        peer_monitor_state,
        validator_peer,
        PeerStateKey::get_all_keys(),
    )
    .await;

    // Verify the new state of the peer monitor
    verify_peer_monitor_state(
        peer_monitor_state,
        validator_peer,
        &connected_peers_and_metadata,
        distance_from_validators,
        1,
    );

    (connected_peers_and_metadata, distance_from_validators)
}

/// Spawns the given task with a timeout
async fn spawn_with_timeout(task: impl Future<Output = ()>, timeout_error_message: &str) {
    let timeout_duration = Duration::from_secs(MAX_WAIT_TIME_SECS);
    timeout(timeout_duration, task)
        .await
        .expect(timeout_error_message)
}

/// Spawns the peer monitor
async fn start_peer_monitor(
    peer_monitoring_client: PeerMonitoringServiceClient<
        NetworkClient<PeerMonitoringServiceMessage>,
    >,
    peer_monitor_state: &PeerMonitorState,
    time_service: &TimeService,
    node_config: &NodeConfig,
) {
    // Spawn the peer monitor state
    tokio::spawn(start_peer_monitor_with_state(
        node_config.clone(),
        peer_monitoring_client,
        peer_monitor_state.clone(),
        time_service.clone(),
        Some(Handle::current()),
    ));

    // Wait for some time so that the peer monitor starts before we return
    sleep(Duration::from_secs(PAUSE_FOR_MONITOR_SETUP_SECS)).await
}

/// Elapses enough time for a latency ping and handles the ping
async fn verify_and_handle_latency_ping(
    mock_monitoring_server: &mut MockMonitoringServer,
    peer_monitor_state: &PeerMonitorState,
    node_config: &NodeConfig,
    peer_network_id: &PeerNetworkId,
    mock_time: &MockTimeService,
    expected_latency_ping_counter: u64,
    expected_num_recorded_latency_pings: u64,
) {
    // Elapse enough time for a latency ping update
    let time_before_update = mock_time.now();
    elapse_latency_update_interval(node_config.clone(), mock_time.clone()).await;

    // Verify that a single latency request is received and respond
    verify_latency_request_and_respond(
        mock_monitoring_server,
        expected_latency_ping_counter,
        false,
        false,
        false,
    )
    .await;

    // Wait until the latency peer state is updated by the client
    wait_for_peer_state_update(
        time_before_update,
        peer_monitor_state,
        peer_network_id,
        vec![PeerStateKey::LatencyInfo],
    )
    .await;

    // Verify the latency ping state
    verify_peer_latency_state(
        peer_monitor_state,
        peer_network_id,
        expected_num_recorded_latency_pings,
        0,
    );
}

/// Verifies that all request types are received by the server
/// and responds to them using the specified data.
async fn verify_all_requests_and_respond(
    mock_monitoring_server: &mut MockMonitoringServer,
    connected_peers_and_metadata: &HashMap<PeerNetworkId, PeerMetadata>,
    distance_from_validators: u64,
) {
    // Create a task that waits for all the requests and sends responses
    let handle_requests = async move {
        // Counters to ensure we only receive one type of each request
        let mut num_received_latency_pings = 0;
        let mut num_received_network_requests = 0;

        // We expect a request to be sent for each peer state type
        let num_state_types = PeerStateKey::get_all_keys().len();
        for _ in 0..num_state_types {
            // Process the peer monitoring request
            let network_request = mock_monitoring_server.next_request().await.unwrap();
            let response = match network_request.peer_monitoring_service_request {
                PeerMonitoringServiceRequest::GetNetworkInformation => {
                    // Increment the counter
                    num_received_network_requests += 1;

                    // Return the response
                    PeerMonitoringServiceResponse::NetworkInformation(NetworkInformationResponse {
                        connected_peers_and_metadata: connected_peers_and_metadata.clone(),
                        distance_from_validators,
                    })
                },
                PeerMonitoringServiceRequest::LatencyPing(latency_ping) => {
                    // Increment the counter
                    num_received_latency_pings += 1;

                    // Return the response
                    PeerMonitoringServiceResponse::LatencyPing(LatencyPingResponse {
                        ping_counter: latency_ping.ping_counter,
                    })
                },
                request => panic!("Unexpected monitoring request received: {:?}", request),
            };

            // Send the response
            network_request.response_sender.send(Ok(response));
        }

        // Verify each request was received exactly once
        if (num_received_latency_pings != 1) || (num_received_network_requests != 1) {
            panic!("The requests were not received exactly once!");
        }
    };

    // Spawn the task with a timeout
    spawn_with_timeout(
        handle_requests,
        "Timed-out while waiting for all the requests!",
    )
    .await;
}

/// Verifies that the peer states is empty
fn verify_empty_peer_states(peer_monitor_state: &PeerMonitorState) {
    assert!(peer_monitor_state.peer_states.read().is_empty());
}

/// Verifies that a latency ping request is received and sends a
/// response based on the given parameters.
async fn verify_latency_request_and_respond(
    mock_monitoring_server: &mut MockMonitoringServer,
    expected_ping_counter: u64,
    respond_with_invalid_counter: bool,
    respond_with_invalid_message: bool,
    skip_sending_a_response: bool,
) {
    // Create a task that waits for the request and sends a response
    let handle_request = async move {
        // Process the latency ping request
        let network_request = mock_monitoring_server.next_request().await.unwrap();
        let response = match network_request.peer_monitoring_service_request {
            PeerMonitoringServiceRequest::LatencyPing(latency_ping) => {
                // Verify the ping counter
                assert_eq!(latency_ping.ping_counter, expected_ping_counter);

                // Create the response
                if respond_with_invalid_counter {
                    // Respond with an invalid ping counter
                    PeerMonitoringServiceResponse::LatencyPing(LatencyPingResponse {
                        ping_counter: 1010101,
                    })
                } else if respond_with_invalid_message {
                    // Respond with the wrong message type
                    PeerMonitoringServiceResponse::ServerProtocolVersion(
                        ServerProtocolVersionResponse { version: 999 },
                    )
                } else {
                    // Send a valid response
                    PeerMonitoringServiceResponse::LatencyPing(LatencyPingResponse {
                        ping_counter: latency_ping.ping_counter,
                    })
                }
            },
            request => panic!("Unexpected monitoring request received: {:?}", request),
        };

        // Send the response
        if !skip_sending_a_response {
            network_request.response_sender.send(Ok(response));
        }
    };

    // Spawn the task with a timeout
    spawn_with_timeout(
        handle_request,
        "Timed-out while waiting for a latency ping request",
    )
    .await;
}

/// Verifies that a network info request is received by the
/// server and sends a response.
async fn verify_network_info_request_and_respond(
    mock_monitoring_server: &mut MockMonitoringServer,
    connected_peers_and_metadata: &HashMap<PeerNetworkId, PeerMetadata>,
    distance_from_validators: u64,
) {
    // Create a task that waits for the request and sends a response
    let handle_request = async move {
        // Process the latency ping request
        let network_request = mock_monitoring_server.next_request().await.unwrap();
        let response = match network_request.peer_monitoring_service_request {
            PeerMonitoringServiceRequest::GetNetworkInformation => {
                PeerMonitoringServiceResponse::NetworkInformation(NetworkInformationResponse {
                    connected_peers_and_metadata: connected_peers_and_metadata.clone(),
                    distance_from_validators,
                })
            },
            request => panic!("Unexpected monitoring request received: {:?}", request),
        };

        // Send the response
        network_request.response_sender.send(Ok(response));
    };

    // Spawn the task with a timeout
    spawn_with_timeout(
        handle_request,
        "Timed-out while waiting for a network info request",
    )
    .await;
}

/// Verifies the latency state of the peer monitor
fn verify_peer_latency_state(
    peer_monitor_state: &PeerMonitorState,
    peer_network_id: &PeerNetworkId,
    expected_num_recorded_latency_pings: u64,
    expected_num_consecutive_failures: u64,
) {
    // Fetch the peer monitoring metadata
    let peer_states = peer_monitor_state.peer_states.read();
    let peer_state = peer_states.get(peer_network_id).unwrap();

    // Verify the latency ping state
    let latency_info_state = peer_state.get_latency_info_state().unwrap();
    assert_eq!(
        latency_info_state.get_recorded_latency_pings().len(),
        expected_num_recorded_latency_pings as usize
    );
    assert_eq!(
        latency_info_state
            .get_request_tracker()
            .read()
            .get_num_consecutive_failures(),
        expected_num_consecutive_failures
    );
}

/// Verifies the state of the peer monitor
fn verify_peer_monitor_state(
    peer_monitor_state: &PeerMonitorState,
    peer_network_id: &PeerNetworkId,
    expected_connected_peers_and_metadata: &HashMap<PeerNetworkId, PeerMetadata>,
    expected_distance_from_validators: u64,
    expected_num_recorded_latency_pings: u64,
) {
    // Verify the latency ping state
    verify_peer_latency_state(
        peer_monitor_state,
        peer_network_id,
        expected_num_recorded_latency_pings,
        0,
    );

    // Verify the network state
    verify_peer_network_state(
        peer_monitor_state,
        peer_network_id,
        expected_connected_peers_and_metadata,
        expected_distance_from_validators,
    );
}

/// Verifies the network state of the peer monitor
fn verify_peer_network_state(
    peer_monitor_state: &PeerMonitorState,
    peer_network_id: &PeerNetworkId,
    expected_connected_peers_and_metadata: &HashMap<PeerNetworkId, PeerMetadata>,
    expected_distance_from_validators: u64,
) {
    // Fetch the peer monitoring metadata
    let peer_states = peer_monitor_state.peer_states.read();
    let peer_state = peer_states.get(peer_network_id).unwrap();

    // Verify the network state
    let network_info_state = peer_state.get_network_info_state().unwrap();
    let latest_network_info_response = network_info_state
        .get_latest_network_info_response()
        .unwrap();
    assert_eq!(
        latest_network_info_response.connected_peers_and_metadata,
        expected_connected_peers_and_metadata.clone()
    );
    assert_eq!(
        latest_network_info_response.distance_from_validators,
        expected_distance_from_validators
    );
}

/// Waits for the peer monitor state to be updated with
/// a latency ping failure.
async fn wait_for_latency_ping_failure(
    peer_monitor_state: &PeerMonitorState,
    peer_network_id: &PeerNetworkId,
    num_expected_consecutive_failures: u64,
) {
    // Create a task that waits for the updated states
    let wait_for_update = async move {
        loop {
            // Fetch the request tracker for the latency state
            let peers_states_lock = peer_monitor_state.peer_states.read();
            let peer_state = peers_states_lock.get(peer_network_id).unwrap();
            let request_tracker = peer_state
                .get_request_tracker(&PeerStateKey::LatencyInfo)
                .unwrap();
            drop(peers_states_lock);

            // Check if the request tracker failures matches the expected number
            let num_consecutive_failures = request_tracker.read().get_num_consecutive_failures();
            if num_consecutive_failures == num_expected_consecutive_failures {
                return; // The peer state was updated!
            }

            // Sleep for some time before retrying
            sleep(Duration::from_millis(SLEEP_DURATION_MS)).await;
        }
    };

    // Spawn the task with a timeout
    spawn_with_timeout(
        wait_for_update,
        "Timed-out while waiting for a latency ping failure!",
    )
    .await;
}

/// Waits for the peer monitor state to be updated with
/// metadata after the given timestamp.
async fn wait_for_peer_state_update(
    time_before_update: Instant,
    peer_monitor_state: &PeerMonitorState,
    peer_network_id: &PeerNetworkId,
    peer_state_keys: Vec<PeerStateKey>,
) {
    // Create a task that waits for the updated states
    let wait_for_update = async move {
        // Go through all peer states and ensure each one is updated
        for peer_state_key in peer_state_keys {
            loop {
                // Fetch the request tracker for the peer state
                let peers_states_lock = peer_monitor_state.peer_states.read();
                let peer_state = peers_states_lock.get(peer_network_id).unwrap();
                let request_tracker = peer_state.get_request_tracker(&peer_state_key).unwrap();
                drop(peers_states_lock);

                // Check if the request tracker has a response with a newer timestamp
                if let Some(last_response_time) = request_tracker.read().get_last_response_time() {
                    if last_response_time > time_before_update {
                        break; // The peer state was updated!
                    }
                };

                // Sleep for some time before retrying
                sleep(Duration::from_millis(SLEEP_DURATION_MS)).await;
            }
        }
    };

    // Spawn the task with a timeout
    spawn_with_timeout(
        wait_for_update,
        "Timed-out while waiting for a peer state update!",
    )
    .await;
}

/// A simple mock of the peer monitoring server for test purposes
struct MockMonitoringServer {
    network_id: NetworkId,
    peer_manager_request_receiver:
        aptos_channel::Receiver<(PeerId, ProtocolId), PeerManagerRequest>,
    peers_and_metadata: Arc<PeersAndMetadata>,
}

impl MockMonitoringServer {
    fn new(
        all_network_ids: Option<Vec<NetworkId>>,
        client_network_id: Option<NetworkId>,
    ) -> (
        PeerMonitoringServiceClient<NetworkClient<PeerMonitoringServiceMessage>>,
        Self,
        PeerMonitorState,
        TimeService,
    ) {
        // Setup the test logger (if it hasn't already been initialized)
        ::aptos_logger::Logger::init_for_testing();

        // Setup the request channels and the network sender
        let queue_config = aptos_channel::Config::new(10).queue_style(QueueStyle::FIFO);
        let (peer_manager_request_sender, peer_manager_request_receiver) = queue_config.build();
        let (connection_request_sender, _connection_request_receiver) = queue_config.build();
        let network_sender = NetworkSender::new(
            PeerManagerRequestSender::new(peer_manager_request_sender),
            ConnectionRequestSender::new(connection_request_sender),
        );

        // Setup the network client
        let networks = all_network_ids
            .unwrap_or_else(|| vec![NetworkId::Validator, NetworkId::Vfn, NetworkId::Public]);
        let peers_and_metadata = PeersAndMetadata::new(&networks);
        let client_network_id = client_network_id.unwrap_or(NetworkId::Validator);
        let network_client = NetworkClient::new(
            vec![], // The peer monitoring service doesn't use direct send
            vec![ProtocolId::PeerMonitoringServiceRpc],
            hashmap! { client_network_id => network_sender },
            peers_and_metadata.clone(),
        );

        // Create the mock server
        let mock_monitoring_server = Self {
            network_id: client_network_id,
            peer_manager_request_receiver,
            peers_and_metadata,
        };

        (
            PeerMonitoringServiceClient::new(network_client),
            mock_monitoring_server,
            PeerMonitorState::new(),
            TimeService::mock(),
        )
    }

    /// Add a new peer to the peers and metadata struct
    fn add_peer_with_network_id(&mut self, network_id: NetworkId, role: PeerRole) -> PeerNetworkId {
        // Create a new peer
        let peer_id = PeerId::random();
        let peer_network_id = PeerNetworkId::new(network_id, peer_id);

        // Create and save a new connection metadata
        let mut connection_metadata = ConnectionMetadata::mock_with_role_and_origin(
            peer_id,
            role,
            ConnectionOrigin::Outbound,
        );
        connection_metadata
            .application_protocols
            .insert(ProtocolId::PeerMonitoringServiceRpc);
        self.peers_and_metadata
            .insert_connection_metadata(peer_network_id, connection_metadata)
            .unwrap();

        // Return the new peer
        peer_network_id
    }

    /// Disconnects the peer in the peers and metadata struct
    fn disconnect_peer(&mut self, peer: PeerNetworkId) {
        self.update_peer_state(peer, ConnectionState::Disconnected);
    }

    /// Reconnects the peer in the peers and metadata struct
    fn reconnect_peer(&mut self, peer: PeerNetworkId) {
        self.update_peer_state(peer, ConnectionState::Connected);
    }

    /// Updates the state of the given peer in the peers and metadata struct
    fn update_peer_state(&mut self, peer: PeerNetworkId, state: ConnectionState) {
        self.peers_and_metadata
            .update_connection_state(peer, state)
            .unwrap();
    }

    /// Get the next request sent from the client
    async fn next_request(&mut self) -> Option<NetworkRequest> {
        match self.peer_manager_request_receiver.next().await {
            Some(PeerManagerRequest::SendRpc(peer_id, network_request)) => {
                // Deconstruct the network request
                let peer_network_id = PeerNetworkId::new(self.network_id, peer_id);
                let protocol_id = network_request.protocol_id;
                let request_data = network_request.data;
                let response_sender = network_request.res_tx;

                // Deserialize the network message
                let peer_monitoring_message: PeerMonitoringServiceMessage =
                    bcs::from_bytes(request_data.as_ref()).unwrap();
                let peer_monitoring_service_request = match peer_monitoring_message {
                    PeerMonitoringServiceMessage::Request(request) => request,
                    _ => panic!("Unexpected message received: {:?}", peer_monitoring_message),
                };

                // Return the network request
                Some(NetworkRequest {
                    peer_network_id,
                    protocol_id,
                    peer_monitoring_service_request,
                    response_sender: ResponseSender::new(response_sender),
                })
            },
            Some(PeerManagerRequest::SendDirectSend(_, _)) => {
                panic!("Unexpected direct send message received!")
            },
            None => None,
        }
    }
}
