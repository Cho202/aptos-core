// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use aptos_time_service::{TimeService, TimeServiceTrait};
use std::{
    ops::Add,
    time::{Duration, Instant},
};

/// A simple container that tracks request and response states
#[derive(Clone, Debug)]
pub struct RequestTracker {
    in_flight_request: bool, // If there is a request currently in-flight
    last_request_time: Option<Instant>, // The most recent request time
    last_response_time: Option<Instant>, // The most recent response time
    num_consecutive_request_failures: u64, // The number of consecutive request failures
    request_interval_ms: u64, // The interval (ms) between requests
    time_service: TimeService, // The time service to use for duration calculation
}

impl RequestTracker {
    pub fn new(request_interval_ms: u64, time_service: TimeService) -> Self {
        Self {
            in_flight_request: false,
            last_request_time: None,
            last_response_time: None,
            num_consecutive_request_failures: 0,
            request_interval_ms,
            time_service,
        }
    }

    /// Returns the last request time
    pub fn get_last_request_time(&self) -> Option<Instant> {
        self.last_request_time
    }

    /// Returns the last response time
    pub fn get_last_response_time(&self) -> Option<Instant> {
        self.last_response_time
    }

    /// Returns the number of consecutive failures
    pub fn get_num_consecutive_failures(&self) -> u64 {
        self.num_consecutive_request_failures
    }

    /// Returns true iff there is a request currently in-flight
    pub fn in_flight_request(&self) -> bool {
        self.in_flight_request
    }

    /// Updates the state to mark a request as having started
    pub fn request_started(&mut self) {
        // Mark the request as in-flight
        self.in_flight_request = true;

        // Update the last request time
        self.last_request_time = Some(self.time_service.now());
    }

    /// Updates the state to mark a request as having completed
    pub fn request_completed(&mut self) {
        self.in_flight_request = false;
    }

    /// Returns true iff a new request should be sent (based
    /// on the latest response time).
    pub fn new_request_required(&self) -> bool {
        // There's already an in-flight request. A new one should not be sent.
        if self.in_flight_request() {
            return false;
        }

        // Otherwise, check the last request time for freshness
        match self.last_request_time {
            Some(last_request_time) => {
                self.time_service.now()
                    > last_request_time.add(Duration::from_millis(self.request_interval_ms))
            },
            None => true, // A request should be sent immediately
        }
    }

    /// Records a successful response for the request
    pub fn record_response_success(&mut self) {
        // Update the last response time
        self.last_response_time = Some(self.time_service.now());

        // Reset the number of consecutive failures
        self.num_consecutive_request_failures = 0;
    }

    /// Records a failure for the request
    pub fn record_response_failure(&mut self) {
        self.num_consecutive_request_failures += 1;
    }
}
