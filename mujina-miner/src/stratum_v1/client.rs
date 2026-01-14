//! Stratum v1 client implementation.
//!
//! This module contains the main client that manages the connection lifecycle,
//! protocol state, and event emission.

use super::connection::Connection;
use super::error::{StratumError, StratumResult};
use super::messages::{ClientCommand, ClientEvent, JsonRpcMessage, SubmitParams};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace, warn};

/// Pool connection configuration.
#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// Pool URL (stratum+tcp://host:port or host:port)
    pub url: String,

    /// Worker username
    pub username: String,

    /// Worker password
    pub password: String,

    /// User agent string
    pub user_agent: String,

    /// Suggested starting difficulty
    ///
    /// If Some, sent via mining.suggest_difficulty after authorization to
    /// request work at an appropriate difficulty for the miner's hashrate.
    /// If None, the pool chooses difficulty.
    pub suggested_difficulty: Option<u64>,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            username: String::new(),
            password: String::new(),
            user_agent: "mujina-miner/0.1.0-alpha".to_string(),
            suggested_difficulty: None,
        }
    }
}

/// Stratum v1 client.
///
/// Manages connection to a mining pool, handles the protocol lifecycle
/// (subscribe, authorize), and emits events for jobs and shares.
///
/// Handles Stratum's interleaved message pattern where notifications can
/// arrive between request/response pairs. During setup (subscribe/authorize),
/// we process notifications inline while waiting for responses.
pub struct StratumV1Client {
    /// Pool configuration
    config: PoolConfig,

    /// Where to send events
    event_tx: mpsc::Sender<ClientEvent>,

    /// Where to receive commands (optional)
    command_rx: Option<mpsc::Receiver<ClientCommand>>,

    /// Shutdown signal
    shutdown: CancellationToken,

    /// Auto-incrementing message ID
    next_id: u64,

    /// Protocol state (filled after subscription)
    state: Option<ProtocolState>,
}

/// Protocol state after successful subscription.
#[derive(Debug)]
struct ProtocolState {
    /// Extranonce1 value from subscription
    extranonce1: String,

    /// Extranonce2 size in bytes
    extranonce2_size: usize,

    /// Current difficulty (if set)
    difficulty: Option<u64>,

    /// Current version mask (if set)
    version_mask: Option<u32>,
}

impl StratumV1Client {
    /// Create a new Stratum v1 client.
    pub fn new(
        config: PoolConfig,
        event_tx: mpsc::Sender<ClientEvent>,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            config,
            event_tx,
            command_rx: None,
            shutdown,
            next_id: 1,
            state: None,
        }
    }

    /// Create a new Stratum v1 client with command channel.
    pub fn with_commands(
        config: PoolConfig,
        event_tx: mpsc::Sender<ClientEvent>,
        command_rx: mpsc::Receiver<ClientCommand>,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            config,
            event_tx,
            command_rx: Some(command_rx),
            shutdown,
            next_id: 1,
            state: None,
        }
    }

    /// Get next message ID and increment counter.
    fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Send a request and wait for its response.
    ///
    /// Sends the request and then loops reading messages from the connection,
    /// handling notifications along the way, until the response arrives.
    /// This handles Stratum's message interleaving during the setup phase.
    ///
    /// Times out after 30 seconds if no response is received. Responds immediately
    /// to shutdown requests.
    async fn send_request(
        &mut self,
        conn: &mut Connection,
        method: &str,
        params: serde_json::Value,
    ) -> StratumResult<JsonRpcMessage> {
        use tokio::time::{timeout, Duration};

        let id = self.next_id();

        // Send request
        let msg = JsonRpcMessage::request(id, method, params);
        conn.write_message(&msg).await?;

        // Loop until we get our response, handling notifications along the way
        // Timeout after 30s to handle unresponsive pools
        timeout(Duration::from_secs(30), async {
            loop {
                tokio::select! {
                    // Read message from pool
                    result = conn.read_message() => {
                        let msg = result?.ok_or(StratumError::Disconnected)?;

                        match msg {
                            JsonRpcMessage::Response { id: resp_id, .. } if resp_id == id => {
                                // This is our response
                                return Ok(msg);
                            }
                            JsonRpcMessage::Response { id: other_id, .. } => {
                                // Response for a different request - shouldn't happen during setup
                                warn!(msg_id = other_id, "Received response for different request");
                            }
                            JsonRpcMessage::Request {
                                id: None,
                                method,
                                params,
                            } => {
                                // Notification - handle it while waiting for our response
                                if let Err(e) = self.handle_notification(&method, &params).await {
                                    warn!(error = %e, "Error handling notification during setup");
                                }
                            }
                            JsonRpcMessage::Request {
                                id: Some(_),
                                method,
                                ..
                            } => {
                                // Request with ID from server (very unusual)
                                warn!(method = %method, "Server sent request during setup");
                            }
                        }
                    }

                    // Shutdown requested
                    _ = self.shutdown.cancelled() => {
                        return Err(StratumError::Disconnected);
                    }
                }
            }
        })
        .await
        .map_err(|_| StratumError::Timeout)?
    }

    /// Configure version rolling support.
    ///
    /// Sends `mining.configure` to request version rolling capability.
    /// Must be called before subscribe. Returns the mask authorized by the pool,
    /// or None if the pool doesn't support version rolling.
    ///
    /// This is an optional extension. If the pool doesn't respond or errors,
    /// we gracefully fall back to mining without version rolling.
    async fn configure_version_rolling(
        &mut self,
        conn: &mut Connection,
    ) -> StratumResult<Option<u32>> {
        use serde_json::json;

        // Request GP bits mask (0x1fffe000 = bits 13-28)
        let result = self
            .send_request(
                conn,
                "mining.configure",
                json!([
                    ["version-rolling"],
                    {"version-rolling.mask": "1fffe000"}
                ]),
            )
            .await;

        // Manual parsing for better error context than serde
        match result {
            Ok(JsonRpcMessage::Response {
                result: Some(result),
                error: None,
                ..
            }) => {
                let obj = result.as_object().ok_or_else(|| {
                    StratumError::InvalidMessage("configure result not an object".to_string())
                })?;

                // Check if version rolling was accepted
                let accepted = obj
                    .get("version-rolling")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                if !accepted {
                    debug!("Pool declined version rolling");
                    return Ok(None);
                }

                // Get the authorized mask
                let mask_str = obj
                    .get("version-rolling.mask")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        StratumError::InvalidMessage("Missing version-rolling.mask".to_string())
                    })?;

                let mask =
                    u32::from_str_radix(mask_str.trim_start_matches("0x"), 16).map_err(|_| {
                        StratumError::InvalidMessage("Invalid version mask hex".to_string())
                    })?;

                debug!(
                    mask = format!("{:#x}", mask),
                    "Pool authorized version rolling"
                );
                Ok(Some(mask))
            }
            Ok(JsonRpcMessage::Response { error: Some(_), .. }) => {
                // Pool returned error - doesn't support mining.configure
                debug!("Pool doesn't support mining.configure (error response)");
                Ok(None)
            }
            Err(StratumError::Timeout) => {
                // Pool didn't respond - doesn't support mining.configure
                debug!("Pool doesn't support mining.configure (timeout)");
                Ok(None)
            }
            Err(e) => Err(e), // Other errors are fatal
            _ => Ok(None),
        }
    }

    /// Subscribe to mining notifications.
    ///
    /// Sends `mining.subscribe` and waits for response containing extranonce1
    /// and extranonce2_size. Uses the message router to handle interleaved
    /// notifications. (TODO: Does this still use a router?)
    async fn subscribe(
        &mut self,
        conn: &mut Connection,
        authorized_mask: Option<u32>,
    ) -> StratumResult<()> {
        use serde_json::json;

        let response = self
            .send_request(conn, "mining.subscribe", json!([&self.config.user_agent]))
            .await?;

        // Parse response
        // Manual parsing for better error context than serde tuple structs
        match response {
            JsonRpcMessage::Response {
                result: Some(result),
                error: None,
                ..
            } => {
                // Result is an array: [[subscriptions...], extranonce1, extranonce2_size]
                let arr = result.as_array().ok_or_else(|| {
                    StratumError::InvalidMessage("subscribe result not an array".to_string())
                })?;

                if arr.len() < 3 {
                    return Err(StratumError::InvalidMessage(
                        "subscribe result too short".to_string(),
                    ));
                }

                let extranonce1 = arr[1].as_str().ok_or_else(|| {
                    StratumError::InvalidMessage("extranonce1 not a string".to_string())
                })?;

                let extranonce2_size = arr[2].as_u64().ok_or_else(|| {
                    StratumError::InvalidMessage("extranonce2_size not a number".to_string())
                })? as usize;

                self.state = Some(ProtocolState {
                    extranonce1: extranonce1.to_string(),
                    extranonce2_size,
                    difficulty: None,
                    version_mask: authorized_mask,
                });

                Ok(())
            }
            JsonRpcMessage::Response {
                error: Some(error), ..
            } => Err(StratumError::SubscriptionFailed(format!("{:?}", error))),
            _ => Err(StratumError::UnexpectedResponse(
                "Invalid subscribe response".to_string(),
            )),
        }
    }

    /// Authorize with the pool.
    ///
    /// Sends `mining.authorize` with username and password. Uses the message
    /// router to handle interleaved notifications.
    async fn authorize(&mut self, conn: &mut Connection) -> StratumResult<()> {
        use serde_json::json;

        let response = self
            .send_request(
                conn,
                "mining.authorize",
                json!([&self.config.username, &self.config.password]),
            )
            .await?;

        // Parse response
        match response {
            JsonRpcMessage::Response {
                result: Some(result),
                error: None,
                ..
            } => {
                let authorized = result.as_bool().unwrap_or(false);
                if authorized {
                    Ok(())
                } else {
                    Err(StratumError::AuthorizationFailed(
                        "Pool returned false".to_string(),
                    ))
                }
            }
            JsonRpcMessage::Response {
                error: Some(error), ..
            } => Err(StratumError::AuthorizationFailed(format!("{:?}", error))),
            _ => Err(StratumError::UnexpectedResponse(
                "Invalid authorize response".to_string(),
            )),
        }
    }

    /// Suggest a difficulty to the pool.
    ///
    /// Sends `mining.suggest_difficulty` as a JSON-RPC notification (no id)
    /// to request work at a specific difficulty. The pool responds with a
    /// `mining.set_difficulty` notification if it accepts the suggestion.
    async fn suggest_difficulty(
        &mut self,
        conn: &mut Connection,
        difficulty: u64,
    ) -> StratumResult<()> {
        use serde_json::json;

        // Send as notification (no id, no response expected)
        let msg = JsonRpcMessage::notification("mining.suggest_difficulty", json!([difficulty]));
        conn.write_message(&msg).await?;

        Ok(())
    }

    /// Submit a share to the pool.
    ///
    /// Sends `mining.submit` and waits for acceptance/rejection. Emits
    /// ShareAccepted or ShareRejected events based on pool response.
    async fn submit(&mut self, conn: &mut Connection, params: SubmitParams) -> StratumResult<bool> {
        use serde_json::Value;

        let job_id = params.job_id.clone();
        let nonce = params.nonce;

        // Convert to Stratum JSON format
        let submit_json = params.to_stratum_json();
        let response = self
            .send_request(conn, "mining.submit", Value::Array(submit_json))
            .await?;

        // Parse response and emit appropriate event
        match response {
            JsonRpcMessage::Response {
                result: Some(result),
                error: None,
                ..
            } => {
                // Result should be true for accepted
                let accepted = result.as_bool().unwrap_or(false);
                if accepted {
                    self.event_tx
                        .send(ClientEvent::ShareAccepted { job_id, nonce })
                        .await
                        .map_err(|_| StratumError::Disconnected)?;
                } else {
                    self.event_tx
                        .send(ClientEvent::ShareRejected {
                            job_id,
                            reason: "Pool returned false".to_string(),
                        })
                        .await
                        .map_err(|_| StratumError::Disconnected)?;
                }
                Ok(accepted)
            }
            JsonRpcMessage::Response {
                error: Some(error), ..
            } => {
                // Pool rejected with error message
                // Error format: [error_code, "error message", null]
                let reason = if let Some(arr) = error.as_array() {
                    arr.get(1)
                        .and_then(|v| v.as_str())
                        .unwrap_or("Unknown error")
                        .to_string()
                } else {
                    format!("{:?}", error)
                };

                self.event_tx
                    .send(ClientEvent::ShareRejected {
                        job_id,
                        reason: reason.clone(),
                    })
                    .await
                    .map_err(|_| StratumError::Disconnected)?;

                Ok(false)
            }
            _ => Err(StratumError::UnexpectedResponse(
                "Invalid submit response".to_string(),
            )),
        }
    }

    /// Handle a notification from the pool.
    async fn handle_notification(
        &mut self,
        method: &str,
        params: &serde_json::Value,
    ) -> StratumResult<()> {
        match method {
            "mining.notify" => {
                self.handle_mining_notify(params).await?;
            }
            "mining.set_difficulty" => {
                self.handle_set_difficulty(params).await?;
            }
            "mining.set_version_mask" => {
                self.handle_set_version_mask(params).await?;
            }
            "client.reconnect" => {
                // Pool is requesting reconnect - treat as disconnection
                return Err(StratumError::Disconnected);
            }
            _ => {
                // Unknown notification - log and ignore
                tracing::warn!(method = %method, "Unknown notification method");
            }
        }
        Ok(())
    }

    /// Handle mining.notify notification.
    async fn handle_mining_notify(&mut self, params: &serde_json::Value) -> StratumResult<()> {
        use super::messages::JobNotification;

        let arr = params.as_array().ok_or_else(|| {
            StratumError::InvalidMessage("mining.notify params not an array".to_string())
        })?;

        let job = JobNotification::from_stratum_params(arr)
            .map_err(|e| StratumError::InvalidMessage(format!("Failed to parse job: {}", e)))?;

        self.event_tx
            .send(ClientEvent::NewJob(job))
            .await
            .map_err(|_| StratumError::Disconnected)?;

        Ok(())
    }

    /// Handle mining.set_difficulty notification.
    async fn handle_set_difficulty(&mut self, params: &serde_json::Value) -> StratumResult<()> {
        // Manual parsing for better error context than serde
        let arr = params.as_array().ok_or_else(|| {
            StratumError::InvalidMessage("set_difficulty params not an array".to_string())
        })?;

        if arr.is_empty() {
            return Err(StratumError::InvalidMessage(
                "set_difficulty params empty".to_string(),
            ));
        }

        let difficulty = arr[0]
            .as_u64()
            .ok_or_else(|| StratumError::InvalidMessage("difficulty not a number".to_string()))?;

        if let Some(state) = &mut self.state {
            state.difficulty = Some(difficulty);
        }

        self.event_tx
            .send(ClientEvent::DifficultyChanged(difficulty))
            .await
            .map_err(|_| StratumError::Disconnected)?;

        Ok(())
    }

    /// Handle mining.set_version_mask notification.
    async fn handle_set_version_mask(&mut self, params: &serde_json::Value) -> StratumResult<()> {
        // Manual parsing for better error context than serde
        let arr = params.as_array().ok_or_else(|| {
            StratumError::InvalidMessage("set_version_mask params not an array".to_string())
        })?;

        if arr.is_empty() {
            return Err(StratumError::InvalidMessage(
                "set_version_mask params empty".to_string(),
            ));
        }

        // Parse hex string to u32
        let mask_str = arr[0]
            .as_str()
            .ok_or_else(|| StratumError::InvalidMessage("version_mask not a string".to_string()))?;

        let mask = u32::from_str_radix(mask_str.trim_start_matches("0x"), 16)
            .map_err(|_| StratumError::InvalidMessage("version_mask not valid hex".to_string()))?;

        if let Some(state) = &mut self.state {
            state.version_mask = Some(mask);
        }

        self.event_tx
            .send(ClientEvent::VersionMaskSet(mask))
            .await
            .map_err(|_| StratumError::Disconnected)?;

        Ok(())
    }

    /// Run the client (main event loop).
    ///
    /// Connects to the pool, subscribes, authorizes, and then enters the main
    /// event loop to handle notifications and submit shares.
    ///
    /// Uses a separate reader task to handle the message router pattern,
    /// allowing requests to wait for responses while processing interleaved
    /// notifications.
    pub async fn run(mut self) -> StratumResult<()> {
        use tracing::{debug, info, warn};

        // Connect
        let mut conn = Connection::connect(&self.config.url).await?;

        // Configure version rolling (before subscribe)
        let authorized_mask = self.configure_version_rolling(&mut conn).await?;

        // Emit configuration result
        self.event_tx
            .send(ClientEvent::VersionRollingConfigured { authorized_mask })
            .await
            .map_err(|_| StratumError::Disconnected)?;

        // Subscribe
        debug!("Subscribing to pool");
        self.subscribe(&mut conn, authorized_mask).await?;

        let state = self.state.as_ref().unwrap();
        debug!(
            extranonce1 = %format_args!("0x{}", state.extranonce1),
            extranonce2_size = %state.extranonce2_size,
            "Subscribed"
        );

        // Emit subscription event with state
        let extranonce1_bytes = hex::decode(&state.extranonce1)
            .map_err(|e| StratumError::InvalidMessage(format!("Invalid extranonce1: {}", e)))?;

        self.event_tx
            .send(ClientEvent::Subscribed {
                extranonce1: extranonce1_bytes,
                extranonce2_size: state.extranonce2_size,
            })
            .await
            .map_err(|_| StratumError::Disconnected)?;

        // Authorize
        self.authorize(&mut conn).await?;
        debug!("Authorized");

        // Suggest difficulty if configured
        if let Some(difficulty) = self.config.suggested_difficulty {
            trace!(difficulty, "Suggesting difficulty to pool");
            if let Err(e) = self.suggest_difficulty(&mut conn, difficulty).await {
                warn!(error = %e, "Failed to suggest difficulty (non-fatal)");
            }
        }

        // Main event loop
        loop {
            tokio::select! {
                // Read messages from pool
                msg = conn.read_message() => {
                    match msg {
                        Ok(Some(msg)) => {
                            // Handle the message
                            match msg {
                                JsonRpcMessage::Request { id: None, method, params } => {
                                    // Notification
                                    if let Err(e) = self.handle_notification(&method, &params).await {
                                        warn!(error = %e, "Error handling notification");
                                        // Non-fatal errors continue
                                        if matches!(e, StratumError::Disconnected) {
                                            return Err(e);
                                        }
                                    }
                                }
                                JsonRpcMessage::Response { id, .. } => {
                                    // Response to a request we sent
                                    // During main loop, we handle responses inline in submit()
                                    // This would be a stray response - log and ignore
                                    debug!(msg_id = %id, "Received unexpected response in main loop");
                                }
                                JsonRpcMessage::Request { id: Some(_), method, .. } => {
                                    // Request with ID from server (unusual, but handle it)
                                    warn!(method = %method, "Server sent request (not notification)");
                                }
                            }
                        }
                        Ok(None) => {
                            // Connection closed
                            info!("Connection closed by pool");
                            self.event_tx.send(ClientEvent::Disconnected).await.ok();
                            return Err(StratumError::Disconnected);
                        }
                        Err(StratumError::InvalidMessage(msg)) => {
                            // Pool sent malformed message (e.g., error response with id=null)
                            // Log as warning but don't disconnect
                            warn!(error = %msg, "Received malformed message from pool, ignoring");
                        }
                        Err(e) => {
                            // Other errors are fatal
                            return Err(e);
                        }
                    }
                }

                // Commands from external code (if command channel exists)
                Some(cmd) = async {
                    match &mut self.command_rx {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    match cmd {
                        ClientCommand::SubmitShare(params) => {
                            debug!(pool = %self.config.url, job_id = %params.job_id, "Submitting share");
                            if let Err(e) = self.submit(&mut conn, params).await {
                                warn!(pool = %self.config.url, error = %e, "Failed to submit share");
                            }
                            // Acceptance/rejection emitted via ShareAccepted/ShareRejected events
                        }
                    }
                }

                // Shutdown signal
                _ = self.shutdown.cancelled() => {
                    self.event_tx.send(ClientEvent::Disconnected).await.ok();
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job_source::Extranonce2;
    use bitcoin::hashes::Hash;
    use tokio::time::{timeout, Duration};

    /// Integration test: Connect to public-pool.io and validate protocol.
    ///
    /// Tests against public-pool.io to validate:
    /// - Connection and subscription
    /// - Job reception and parsing
    /// - Difficulty handling
    ///
    /// # Running Integration Tests
    ///
    /// These tests are ignored by default (require network). Run individually:
    ///
    /// ```bash
    /// # Run this test with output
    /// cargo test --lib test_integration_public_pool -- --ignored --nocapture
    ///
    /// # Run all pool integration tests
    /// cargo test --lib stratum_v1::client::tests::test_integration -- --ignored --nocapture
    /// ```
    #[tokio::test]
    #[ignore]
    async fn test_integration_public_pool() {
        test_pool_integration(
            "public-pool.io:21496",
            "bc1qce93hy5rhg02s6aeu7mfdvxg76x66pqqtrvzs3.mujina-integration-test",
        )
        .await;
    }

    /// Integration test: Connect to ckpool and validate protocol.
    ///
    /// Tests against solo.ckpool.org to validate protocol compatibility.
    ///
    /// See [`test_integration_public_pool`] for running instructions.
    #[tokio::test]
    #[ignore]
    async fn test_integration_ckpool() {
        test_pool_integration(
            "solo.ckpool.org:3333",
            "bc1qce93hy5rhg02s6aeu7mfdvxg76x66pqqtrvzs3.mujina-integration-test",
        )
        .await;
    }

    /// Integration test: Connect to Ocean and validate protocol.
    ///
    /// Tests against mine.ocean.xyz to validate protocol compatibility.
    ///
    /// See [`test_integration_public_pool`] for running instructions.
    #[tokio::test]
    #[ignore]
    async fn test_integration_ocean() {
        test_pool_integration(
            "mine.ocean.xyz:3334",
            "bc1qce93hy5rhg02s6aeu7mfdvxg76x66pqqtrvzs3.mujina-integration-test",
        )
        .await;
    }

    /// Integration test: Connect using environment variables.
    ///
    /// Tests against a pool specified by environment variables. Uses the same
    /// environment variables as the daemon for consistency.
    ///
    /// # Environment Variables
    ///
    /// - `MUJINA_POOL_URL` - Pool URL (required, e.g., "stratum+tcp://localhost:3333")
    /// - `MUJINA_POOL_USER` - Username/wallet address (optional, defaults to test address)
    ///
    /// # Running
    ///
    /// ```bash
    /// # Minimal (uses default test address)
    /// MUJINA_POOL_URL="stratum+tcp://localhost:3333" \
    /// cargo test --lib test_pool_from_env -- --ignored --nocapture
    ///
    /// # With custom username
    /// MUJINA_POOL_URL="stratum+tcp://localhost:3333" \
    /// MUJINA_POOL_USER="bc1qce93hy5rhg02s6aeu7mfdvxg76x66pqqtrvzs3.my-worker" \
    /// cargo test --lib test_pool_from_env -- --ignored --nocapture
    /// ```
    ///
    /// Note: This test uses a different name pattern (`test_pool_from_env`)
    /// so it doesn't run with the standard pool tests.
    #[tokio::test]
    #[ignore]
    async fn test_pool_from_env() {
        let pool_url =
            std::env::var("MUJINA_POOL_URL").expect("MUJINA_POOL_URL environment variable not set");
        let username = std::env::var("MUJINA_POOL_USER").unwrap_or_else(|_| {
            "bc1qce93hy5rhg02s6aeu7mfdvxg76x66pqqtrvzs3.mujina-integration-test".to_string()
        });

        // Strip stratum+tcp:// prefix if present for test_pool_integration
        let url_without_scheme = pool_url.strip_prefix("stratum+tcp://").unwrap_or(&pool_url);

        println!("\n=== Testing custom pool configuration ===");
        println!("Pool: {}", pool_url);
        println!("Username: {}", username);

        test_pool_integration(url_without_scheme, &username).await;
    }

    /// Common integration test logic for pool connections.
    ///
    /// # Test Strategy
    ///
    /// This test validates that the client can successfully connect to a pool and
    /// maintain a stable connection through the complete handshake sequence:
    ///
    /// 1. **Connect**: Establish TCP connection and spawn client task
    /// 2. **Collect events**: Wait for subscription, job, and difficulty events
    /// 3. **Stability check**: Wait 5 seconds to ensure pool doesn't disconnect
    ///
    /// The stability check (phase 3) was added after discovering that Ocean pool
    /// silently disconnects clients that suggest a difficulty lower than their
    /// minimum. We now avoid suggesting difficulty altogether, but the stability
    /// check remains valuable for catching delayed disconnects from pools that
    /// reject our configuration after the initial handshake.
    async fn test_pool_integration(pool_url: &str, username: &str) {
        use tracing_subscriber::{fmt, EnvFilter};

        // Initialize logging for the test
        let _ = fmt()
            .with_env_filter(
                EnvFilter::from_default_env().add_directive("mujina_miner=warn".parse().unwrap()),
            )
            .try_init();

        let (event_tx, mut event_rx) = mpsc::channel(100);
        let shutdown = CancellationToken::new();

        let config = PoolConfig {
            url: format!("stratum+tcp://{}", pool_url),
            username: username.to_string(),
            password: "x".to_string(),
            user_agent: "mujina-miner/0.1.0-test".to_string(),
            suggested_difficulty: None,
        };

        println!("\n=== Connecting to {} ===", pool_url);

        let client = StratumV1Client::new(config, event_tx, shutdown.clone());

        // Phase 1: Connect to pool
        let client_handle = tokio::spawn(async move { client.run().await });

        // Helper to handle and print events
        let handle_event = |event: &ClientEvent,
                            subscribed: &mut bool,
                            received_job: &mut bool,
                            received_difficulty: &mut bool| {
            match event {
                ClientEvent::VersionRollingConfigured { authorized_mask } => {
                    println!("\n[Version Rolling]");
                    if let Some(mask) = authorized_mask {
                        println!("  Authorized mask: {:#010x}", mask);
                    } else {
                        println!("  Not supported");
                    }
                }
                ClientEvent::Subscribed {
                    extranonce1,
                    extranonce2_size,
                } => {
                    println!("\n[Subscribed]");
                    println!("  Extranonce1: {}", hex::encode(extranonce1));
                    println!("  Extranonce2 size: {} bytes", extranonce2_size);
                    assert!(!extranonce1.is_empty(), "extranonce1 should not be empty");
                    assert!(
                        Extranonce2::new(0, *extranonce2_size as u8).is_ok(),
                        "pool returned invalid extranonce2_size: {}",
                        extranonce2_size
                    );
                    *subscribed = true;
                }
                ClientEvent::NewJob(job) => {
                    println!("\n[New Job]");
                    println!("  Job ID: {}", job.job_id);
                    println!("  Previous hash: {}", job.prev_hash);
                    println!("  Version: {:#010x}", job.version.to_consensus());
                    println!("  Nbits: {:#010x}", job.nbits.to_consensus());
                    println!("  Ntime: {} ({})", job.ntime, job.ntime);
                    println!("  Merkle branches: {}", job.merkle_branches.len());
                    println!("  Coinbase1 size: {} bytes", job.coinbase1.len());
                    println!("  Coinbase2 size: {} bytes", job.coinbase2.len());
                    println!("  Clean jobs: {}", job.clean_jobs);
                    assert!(!job.job_id.is_empty(), "job_id should not be empty");
                    assert_eq!(
                        job.prev_hash.as_byte_array().len(),
                        32,
                        "prev_hash should be 32 bytes"
                    );
                    assert!(!job.coinbase1.is_empty(), "coinbase1 should not be empty");
                    assert!(!job.coinbase2.is_empty(), "coinbase2 should not be empty");
                    *received_job = true;
                }
                ClientEvent::DifficultyChanged(diff) => {
                    println!("\n[Difficulty Changed]");
                    println!("  Difficulty: {}", diff);
                    assert!(*diff > 0, "difficulty should be positive");
                    *received_difficulty = true;
                }
                ClientEvent::VersionMaskSet(mask) => {
                    println!("\n[Version Mask Set]");
                    println!("  Mask: {:#010x}", mask);
                }
                ClientEvent::Error(err) => {
                    println!("\n[Error] {}", err);
                }
                _ => {}
            }
        };

        // Phase 2: Collect required events from the pool
        let mut subscribed = false;
        let mut received_job = false;
        let mut received_difficulty = false;

        let result = timeout(Duration::from_secs(10), async {
            loop {
                match event_rx.recv().await {
                    Some(ClientEvent::Disconnected) => {
                        println!("\n[Disconnected]");
                        panic!("Pool disconnected before receiving all required events");
                    }
                    Some(event) => {
                        handle_event(
                            &event,
                            &mut subscribed,
                            &mut received_job,
                            &mut received_difficulty,
                        );
                        if subscribed && received_job && received_difficulty {
                            break;
                        }
                    }
                    None => panic!("Event channel closed before receiving all required events"),
                }
            }

            // Phase 3: Stability check - wait 5 seconds to ensure pool doesn't disconnect
            println!("\n=== All expected events received, waiting for connection stability ===");
            let stability_check = timeout(Duration::from_secs(5), async {
                loop {
                    match event_rx.recv().await {
                        Some(ClientEvent::Disconnected) => {
                            println!("\n[Disconnected]");
                            return Err("disconnected");
                        }
                        Some(event) => {
                            handle_event(
                                &event,
                                &mut subscribed,
                                &mut received_job,
                                &mut received_difficulty,
                            );
                        }
                        None => return Err("channel closed"),
                    }
                }
            })
            .await;

            match stability_check {
                Err(_elapsed) => {
                    // Timeout is success - connection stayed stable for 5 seconds
                    println!("\n=== Connection stable for 5 seconds, test passed ===");
                    Ok(())
                }
                Ok(Err(reason)) => Err(reason),
                Ok(Ok(())) => unreachable!("stability loop never returns Ok"),
            }
        })
        .await;

        // Shutdown client
        shutdown.cancel();
        let _ = client_handle.await;

        // Verify test passed
        assert!(result.is_ok(), "Test timed out waiting for pool events");
        assert!(
            result.unwrap().is_ok(),
            "Pool disconnected too shortly after subscribing"
        );

        println!("\n=== Test complete ===");
    }

    /// Helper to create a minimal client for testing notification handlers.
    fn test_client() -> (StratumV1Client, mpsc::Receiver<ClientEvent>) {
        let (event_tx, event_rx) = mpsc::channel(10);
        let shutdown = CancellationToken::new();

        let config = PoolConfig {
            url: "test:3333".to_string(),
            username: "test".to_string(),
            password: "x".to_string(),
            user_agent: "test".to_string(),
            suggested_difficulty: Some(1024),
        };

        let client = StratumV1Client::new(config, event_tx, shutdown);
        (client, event_rx)
    }

    #[tokio::test]
    async fn test_handle_set_difficulty_valid() {
        use serde_json::json;

        let (mut client, mut event_rx) = test_client();

        let params = json!([2048]);
        let result = client.handle_set_difficulty(&params).await;

        assert!(result.is_ok());

        // Verify event was emitted
        let event = event_rx
            .try_recv()
            .expect("Expected DifficultyChanged event");
        match event {
            ClientEvent::DifficultyChanged(diff) => {
                assert_eq!(diff, 2048);
            }
            _ => panic!("Expected DifficultyChanged event, got {:?}", event),
        }
    }

    #[tokio::test]
    async fn test_handle_set_difficulty_invalid_params() {
        use serde_json::json;

        let (mut client, _event_rx) = test_client();

        // Empty array
        let params = json!([]);
        let result = client.handle_set_difficulty(&params).await;
        assert!(result.is_err());

        // Not an array
        let params = json!(2048);
        let result = client.handle_set_difficulty(&params).await;
        assert!(result.is_err());

        // Not a number
        let params = json!(["not a number"]);
        let result = client.handle_set_difficulty(&params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_handle_set_version_mask_valid() {
        use serde_json::json;

        let (mut client, mut event_rx) = test_client();

        // Without 0x prefix
        let params = json!(["1fffe000"]);
        let result = client.handle_set_version_mask(&params).await;
        assert!(result.is_ok());

        let event = event_rx.try_recv().expect("Expected VersionMaskSet event");
        match event {
            ClientEvent::VersionMaskSet(mask) => {
                assert_eq!(mask, 0x1fffe000);
            }
            _ => panic!("Expected VersionMaskSet event, got {:?}", event),
        }
    }

    #[tokio::test]
    async fn test_handle_set_version_mask_with_0x_prefix() {
        use serde_json::json;

        let (mut client, mut event_rx) = test_client();

        // With 0x prefix
        let params = json!(["0x1fffe000"]);
        let result = client.handle_set_version_mask(&params).await;
        assert!(result.is_ok());

        let event = event_rx.try_recv().expect("Expected VersionMaskSet event");
        match event {
            ClientEvent::VersionMaskSet(mask) => {
                assert_eq!(mask, 0x1fffe000);
            }
            _ => panic!("Expected VersionMaskSet event, got {:?}", event),
        }
    }

    #[tokio::test]
    async fn test_handle_set_version_mask_invalid_params() {
        use serde_json::json;

        let (mut client, _event_rx) = test_client();

        // Empty array
        let params = json!([]);
        let result = client.handle_set_version_mask(&params).await;
        assert!(result.is_err());

        // Not an array
        let params = json!("1fffe000");
        let result = client.handle_set_version_mask(&params).await;
        assert!(result.is_err());

        // Invalid hex
        let params = json!(["not-hex"]);
        let result = client.handle_set_version_mask(&params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_submit_share_accepted() {
        use super::super::connection::Connection;
        use serde_json::json;
        use tokio::net::TcpListener;

        let (mut client, mut event_rx) = test_client();

        // Create mock server
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut conn = Connection::new(socket);

            // Read the submit request
            let msg = conn.read_message().await.unwrap().unwrap();

            // Send acceptance response
            let response = JsonRpcMessage::Response {
                id: msg.id().unwrap(),
                result: Some(json!(true)),
                error: None,
            };
            conn.write_message(&response).await.unwrap();
        });

        // Connect to mock server
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut conn = Connection::new(stream);

        let params = SubmitParams {
            username: "worker".to_string(),
            job_id: "job123".to_string(),
            extranonce2: vec![0x01, 0x02, 0x03, 0x04],
            ntime: 0x12345678,
            nonce: 0xdeadbeef,
            version_bits: Some(0x20000000),
        };

        let accepted = client.submit(&mut conn, params).await.unwrap();
        assert!(accepted);

        // Verify ShareAccepted event was emitted
        let event = event_rx.try_recv().expect("Expected ShareAccepted event");
        match event {
            ClientEvent::ShareAccepted { job_id, nonce } => {
                assert_eq!(job_id, "job123");
                assert_eq!(nonce, 0xdeadbeef);
            }
            _ => panic!("Expected ShareAccepted, got {:?}", event),
        }
    }

    #[tokio::test]
    async fn test_submit_share_rejected_with_error() {
        use super::super::connection::Connection;
        use serde_json::json;
        use tokio::net::TcpListener;

        let (mut client, mut event_rx) = test_client();

        // Create mock server
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut conn = Connection::new(socket);

            // Read the submit request
            let msg = conn.read_message().await.unwrap().unwrap();

            // Send rejection response with error
            let response = JsonRpcMessage::Response {
                id: msg.id().unwrap(),
                result: None,
                error: Some(json!([23, "Low difficulty share", null])),
            };
            conn.write_message(&response).await.unwrap();
        });

        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut conn = Connection::new(stream);

        let params = SubmitParams {
            username: "worker".to_string(),
            job_id: "job456".to_string(),
            extranonce2: vec![0x01, 0x02, 0x03, 0x04],
            ntime: 0x12345678,
            nonce: 0xdeadbeef,
            version_bits: None,
        };

        let accepted = client.submit(&mut conn, params).await.unwrap();
        assert!(!accepted);

        // Verify ShareRejected event was emitted with reason
        let event = event_rx.try_recv().expect("Expected ShareRejected event");
        match event {
            ClientEvent::ShareRejected { job_id, reason } => {
                assert_eq!(job_id, "job456");
                assert_eq!(reason, "Low difficulty share");
            }
            _ => panic!("Expected ShareRejected, got {:?}", event),
        }
    }

    #[tokio::test]
    async fn test_submit_share_rejected_with_false_result() {
        use super::super::connection::Connection;
        use serde_json::json;
        use tokio::net::TcpListener;

        let (mut client, mut event_rx) = test_client();

        // Create mock server
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut conn = Connection::new(socket);

            let msg = conn.read_message().await.unwrap().unwrap();

            // Send rejection as false result (some pools do this)
            let response = JsonRpcMessage::Response {
                id: msg.id().unwrap(),
                result: Some(json!(false)),
                error: None,
            };
            conn.write_message(&response).await.unwrap();
        });

        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut conn = Connection::new(stream);

        let params = SubmitParams {
            username: "worker".to_string(),
            job_id: "job789".to_string(),
            extranonce2: vec![0x01, 0x02, 0x03, 0x04],
            ntime: 0x12345678,
            nonce: 0xdeadbeef,
            version_bits: None,
        };

        let accepted = client.submit(&mut conn, params).await.unwrap();
        assert!(!accepted);

        // Verify ShareRejected event was emitted
        let event = event_rx.try_recv().expect("Expected ShareRejected event");
        match event {
            ClientEvent::ShareRejected { job_id, reason } => {
                assert_eq!(job_id, "job789");
                assert_eq!(reason, "Pool returned false");
            }
            _ => panic!("Expected ShareRejected, got {:?}", event),
        }
    }
}
