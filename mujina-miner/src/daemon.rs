//! Daemon lifecycle management for mujina-miner.
//!
//! This module handles the core daemon functionality including initialization,
//! task management, signal handling, and graceful shutdown.

use std::env;

use tokio::signal::unix::{self, SignalKind};
use tokio::sync::{mpsc, watch};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::api_client::types::MinerTelemetry;
use crate::tracing::prelude::*;
use crate::{
    api::{self, ApiConfig, commands::SchedulerCommand},
    backplane::Backplane,
    board::bzm2::Bzm2RuntimeConfig,
    cpu_miner::CpuMinerConfig,
    job_source::{
        SourceCommand, SourceEvent,
        dummy::DummySource,
        forced_rate::{ForcedRateConfig, ForcedRateSource},
        stratum_v1::StratumV1Source,
    },
    scheduler::{self, SourceRegistration, ThreadRegistration},
    stratum_v1::{PoolConfig as StratumPoolConfig, TcpConnector},
    transport::{CpuDeviceInfo, TransportEvent, UsbTransport, cpu as cpu_transport},
};

/// The main daemon.
pub struct Daemon {
    shutdown: CancellationToken,
    tracker: TaskTracker,
}

impl Daemon {
    /// Create a new daemon instance.
    pub fn new() -> Self {
        Self {
            shutdown: CancellationToken::new(),
            tracker: TaskTracker::new(),
        }
    }

    /// Run the daemon until shutdown is requested.
    pub async fn run(self) -> anyhow::Result<()> {
        // Observer mode: attach and enumerate hardware, stream telemetry, serve
        // read-only diagnostics, and never dispatch work. Intended for a
        // machine whose hashboards share one PSU rail, where an unasked-for
        // job would draw current the operator did not budget for. Two
        // independent guarantees, because one switch that half-works is worse
        // than none: no job source is created at all, and the scheduler starts
        // paused, so even a source arriving another way assigns nothing.
        // READ THE VALUE, NOT THE PRESENCE. This was `.is_ok()`, the only
        // switch in the tree read that way, so `MUJINA_OBSERVE=0` turned
        // observer mode ON. The first run planned to dispatch work set exactly
        // that, and would have dispatched nothing while reporting nothing
        // wrong -- a mining run that silently cannot mine.
        let observe = observe_requested(std::env::var("MUJINA_OBSERVE").ok().as_deref());
        if observe {
            warn!("Observer mode (MUJINA_OBSERVE): no job source, mining starts paused");
        }

        // Create channels for component communication. Each transport gets its
        // own event channel; the backplane waits for one enumeration completion
        // per channel.
        let (thread_tx, thread_rx) = mpsc::channel::<ThreadRegistration>(10);
        let (source_reg_tx, source_reg_rx) = mpsc::channel::<SourceRegistration>(10);
        let mut transport_rxs: Vec<mpsc::Receiver<TransportEvent>> = Vec::new();

        // Create and start USB transport discovery
        if std::env::var("MUJINA_USB_DISABLE").is_err() {
            let (usb_tx, usb_rx) = mpsc::channel::<TransportEvent>(100);
            let usb_transport = UsbTransport::new(usb_tx);
            if let Err(e) = usb_transport.start_discovery(self.shutdown.clone()).await {
                error!("Failed to start USB discovery: {}", e);
            }
            transport_rxs.push(usb_rx);
        } else {
            info!("USB discovery disabled (MUJINA_USB_DISABLE set)");
        }

        // Inject CPU miner virtual device if configured
        if let Some(config) = CpuMinerConfig::from_env() {
            info!(
                threads = config.thread_count,
                duty = config.duty_percent,
                "CPU miner enabled"
            );
            let (cpu_tx, cpu_rx) = mpsc::channel::<TransportEvent>(100);
            let device = TransportEvent::Cpu(cpu_transport::TransportEvent::CpuDeviceConnected(
                CpuDeviceInfo {
                    device_id: format!("cpu-{}x{}%", config.thread_count, config.duty_percent),
                    thread_count: config.thread_count,
                    duty_percent: config.duty_percent,
                },
            ));
            // Send the device and its enumeration completion, then drop the
            // sender; the CPU transport has no further events.
            if let Err(e) = cpu_tx.send(device).await {
                error!("Failed to send CPU miner event: {}", e);
            }
            let _ = cpu_tx
                .send(TransportEvent::InitialEnumerationComplete)
                .await;
            transport_rxs.push(cpu_rx);
        }

        // Board registration channel: backplane forwards board
        // registrations here, the API server collects and serves them.
        let (board_reg_tx, board_reg_rx) = mpsc::channel(10);

        // Create and start backplane
        let mut backplane = Backplane::new(transport_rxs, thread_tx, board_reg_tx);

        // Attach a configured BZM2 board before the backplane starts draining
        // transport events, so its threads register ahead of the
        // initial-enumeration-complete signal and count toward the startup
        // hold.
        if let Some(config) = Bzm2RuntimeConfig::from_env() {
            info!(
                serials = config.serial_paths.len(),
                baud = config.baud_rate,
                "BZM2 board enabled from configured serial paths"
            );
            backplane
                .attach_configured_board("bzm2", config.device_id())
                .await?;
        }

        self.tracker.spawn({
            let shutdown = self.shutdown.clone();
            async move {
                tokio::select! {
                    result = backplane.run() => {
                        if let Err(e) = result {
                            error!("Backplane error: {}", e);
                        }
                    }
                    _ = shutdown.cancelled() => {}
                }

                // The event loop ending is not the daemon ending.
                //
                // `run()` multiplexes per-transport event streams. With no
                // transports -- the embedded case, where USB discovery is
                // compiled out or disabled and boards come from configuration
                // instead -- there is nothing to multiplex, so it returns on
                // its first poll. Falling straight through to the teardown
                // below destroyed every configured board a few milliseconds
                // after it was attached, while the daemon stayed up serving an
                // empty board list and 404s.
                //
                // A board attached by configuration outlives the loop that
                // never carried it. Only cancellation ends it.
                if !shutdown.is_cancelled() {
                    debug!("Backplane event loop finished; holding boards until shutdown");
                    shutdown.cancelled().await;
                }

                backplane.shutdown_all_boards().await;
            }
        });

        // Create job source (Stratum v1 or Dummy)
        // Controlled by environment variables:
        // - MUJINA_POOL_URL: Pool address (e.g., stratum+tcp://localhost:3333)
        // - MUJINA_POOL_USER: Worker username (optional, defaults to "mujina-testing")
        // - MUJINA_POOL_PASS: Worker password (optional, defaults to "x")
        let (source_event_tx, source_event_rx) = mpsc::channel::<SourceEvent>(100);
        let (source_cmd_tx, source_cmd_rx) = mpsc::channel(10);

        if let Ok(pool_url) = env::var("MUJINA_POOL_URL") {
            // Use Stratum v1 source
            let pool_user =
                env::var("MUJINA_POOL_USER").unwrap_or_else(|_| "mujina-testing".to_string());
            let pool_pass = env::var("MUJINA_POOL_PASS").unwrap_or_else(|_| "x".to_string());

            let stratum_config = StratumPoolConfig {
                url: pool_url.clone(),
                username: pool_user,
                password: pool_pass,
                user_agent: "mujina-miner/0.1.0-alpha".to_string(),
            };

            // Optionally wrap with ForcedRateSource for testing
            if let Some(forced_rate_config) = ForcedRateConfig::from_env() {
                info!(
                    rate = %forced_rate_config.target_rate,
                    "Forced share rate wrapper enabled"
                );

                // Create inner channels (stratum <-> wrapper)
                let (inner_event_tx, inner_event_rx) = mpsc::channel::<SourceEvent>(100);
                let (inner_cmd_tx, inner_cmd_rx) = mpsc::channel::<SourceCommand>(10);

                let stratum_source = StratumV1Source::new(
                    stratum_config,
                    inner_cmd_rx,
                    inner_event_tx,
                    self.shutdown.clone(),
                    Box::new(TcpConnector::new(pool_url.clone())),
                );
                let stratum_name = stratum_source.name();

                // Spawn stratum source
                self.tracker.spawn(async move {
                    if let Err(e) = stratum_source.run().await {
                        error!("Stratum v1 source error: {}", e);
                    }
                });

                // Create and spawn wrapper (uses outer channels from above)
                let forced_rate = ForcedRateSource::new(
                    forced_rate_config,
                    inner_event_rx,
                    source_event_tx,
                    inner_cmd_tx,
                    source_cmd_rx,
                    self.shutdown.clone(),
                );

                source_reg_tx
                    .send(SourceRegistration {
                        name: format!("{} (forced-rate)", stratum_name),
                        url: Some(pool_url.clone()),
                        event_rx: source_event_rx,
                        command_tx: source_cmd_tx,
                    })
                    .await?;

                self.tracker.spawn(async move {
                    if let Err(e) = forced_rate.run().await {
                        error!("Forced rate wrapper error: {}", e);
                    }
                });
            } else {
                // Direct stratum source (no wrapper)
                let stratum_source = StratumV1Source::new(
                    stratum_config,
                    source_cmd_rx,
                    source_event_tx,
                    self.shutdown.clone(),
                    Box::new(TcpConnector::new(pool_url.clone())),
                );

                source_reg_tx
                    .send(SourceRegistration {
                        name: stratum_source.name(),
                        url: Some(pool_url),
                        event_rx: source_event_rx,
                        command_tx: source_cmd_tx,
                    })
                    .await?;

                self.tracker.spawn(async move {
                    if let Err(e) = stratum_source.run().await {
                        error!("Stratum v1 source error: {}", e);
                    }
                });
            }
        } else if observe {
            info!("Observer mode: no job source registered");
        } else {
            // Use DummySource
            info!("Using dummy job source (set MUJINA_POOL_URL to use Stratum v1)");

            let dummy_source = DummySource::new(
                source_cmd_rx,
                source_event_tx,
                self.shutdown.clone(),
                tokio::time::Duration::from_secs(30),
            )?;

            source_reg_tx
                .send(SourceRegistration {
                    name: "dummy".into(),
                    url: None,
                    event_rx: source_event_rx,
                    command_tx: source_cmd_tx,
                })
                .await?;

            self.tracker.spawn(async move {
                if let Err(e) = dummy_source.run().await {
                    error!("DummySource error: {}", e);
                }
            });
        }

        // Miner state channel: scheduler publishes snapshots, API serves them.
        let (miner_telemetry_tx, miner_telemetry_rx) = watch::channel(MinerTelemetry::default());

        // Command channel: API sends commands, scheduler processes them.
        let (scheduler_cmd_tx, scheduler_cmd_rx) = mpsc::channel::<SchedulerCommand>(16);

        // Start the scheduler
        self.tracker.spawn(scheduler::task(
            self.shutdown.clone(),
            thread_rx,
            source_reg_rx,
            miner_telemetry_tx,
            scheduler_cmd_rx,
            observe,
        ));

        // Start the API server
        self.tracker.spawn({
            let shutdown = self.shutdown.clone();
            async move {
                // ASCII 'M' (77) + 'U' (85) = 7785
                const API_PORT: u16 = 7785;

                let bind_addr = match env::var("MUJINA_API_LISTEN") {
                    Ok(addr) if addr.contains(':') => addr,
                    Ok(addr) => format!("{addr}:{API_PORT}"),
                    Err(_) => format!("127.0.0.1:{API_PORT}"),
                };
                let env_flag = |key: &str| {
                    env::var(key)
                        .ok()
                        .map(|v| {
                            let v = v.trim().to_ascii_lowercase();
                            v == "1" || v == "true" || v == "yes" || v == "on"
                        })
                        .unwrap_or(false)
                };
                let config = ApiConfig {
                    bind_addr,
                    raw_registers_enabled: env_flag("MUJINA_API_RAW_REGISTERS"),
                    raw_registers_allow_remote: env_flag("MUJINA_API_RAW_REGISTERS_ALLOW_REMOTE"),
                };
                if let Err(e) = api::serve(
                    config,
                    shutdown,
                    miner_telemetry_rx,
                    board_reg_rx,
                    scheduler_cmd_tx,
                )
                .await
                {
                    error!("API server error: {}", e);
                }
            }
        });

        self.tracker.close();

        info!("Started.");
        info!("For debugging, set MUJINA_LOG=debug or trace.");

        // Install signal handlers
        let mut sigint = unix::signal(SignalKind::interrupt())?;
        let mut sigterm = unix::signal(SignalKind::terminate())?;

        // Wait for shutdown signal
        tokio::select! {
            _ = sigint.recv() => {
                info!("Received SIGINT.");
            },
            _ = sigterm.recv() => {
                info!("Received SIGTERM.");
            },
        }

        // Initiate shutdown
        self.shutdown.cancel();

        // Wait for all tasks to complete
        self.tracker.wait().await;
        info!("Exiting.");

        Ok(())
    }
}

impl Default for Daemon {
    fn default() -> Self {
        Self::new()
    }
}

/// Is observer mode requested, given the raw `MUJINA_OBSERVE` value?
///
/// FAILS TOWARD OBSERVING. Unset means dispatch, as it always has, and the
/// explicit falsy spellings mean dispatch. Anything this does not recognise --
/// a typo, `O` for `0`, `flase` -- means OBSERVE, loudly. The rest of the tree's
/// flags treat an unknown value as false, which is right for them and wrong
/// here: for this switch false means "send work to a 3 kW machine", and a
/// mistyped value must not be the thing that starts it hashing.
pub(crate) fn observe_requested(value: Option<&str>) -> bool {
    let Some(raw) = value else { return false };
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => true,
        "0" | "false" | "no" | "off" | "" => false,
        other => {
            tracing::warn!(
                value = other,
                "MUJINA_OBSERVE has a value that is neither on nor off; treating it as ON -- \
                 observing, dispatching nothing -- because an unreadable switch must not be \
                 the one that starts the machine hashing"
            );
            true
        }
    }
}

#[cfg(test)]
mod observe_switch_tests {
    use super::observe_requested;

    /// The defect: presence, not value. `MUJINA_OBSERVE=0` meant observe.
    #[test]
    fn zero_means_dispatch_not_observe() {
        assert!(
            !observe_requested(Some("0")),
            "=0 must NOT enable observer mode"
        );
        assert!(!observe_requested(Some("false")));
        assert!(!observe_requested(Some("off")));
        assert!(!observe_requested(Some("")));
    }

    #[test]
    fn unset_means_dispatch_as_before() {
        assert!(!observe_requested(None));
    }

    #[test]
    fn the_on_spellings_observe() {
        for v in ["1", "true", "YES", " on "] {
            assert!(observe_requested(Some(v)), "{v:?}");
        }
    }

    /// An unreadable value fails toward NOT making heat.
    #[test]
    fn an_unrecognised_value_observes() {
        assert!(
            observe_requested(Some("O")),
            "a typo for 0 must not start hashing"
        );
        assert!(observe_requested(Some("flase")));
    }
}
