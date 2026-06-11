//! HTTP server lifecycle and router construction.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use axum::{Router, response::Redirect, routing};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use tower_http::trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer};
use tracing::Level;

use crate::tracing::prelude::*;
use utoipa_axum::router::OpenApiRouter;
use utoipa_swagger_ui::SwaggerUi;

use super::{
    commands::SchedulerCommand,
    registry::{BoardRegistration, BoardRegistry},
    v0,
};
use crate::api_client::types::MinerTelemetry;

/// API server configuration.
#[derive(Debug, Clone)]
pub struct ApiConfig {
    /// Address and port to bind the API server to.
    pub bind_addr: String,
}

/// Shared application state available to all handlers.
#[derive(Clone)]
pub(crate) struct SharedState {
    pub miner_telemetry_rx: watch::Receiver<MinerTelemetry>,
    pub board_registry: Arc<Mutex<BoardRegistry>>,
    pub scheduler_cmd_tx: mpsc::Sender<SchedulerCommand>,
}

impl SharedState {
    /// Build a complete MinerTelemetry by combining scheduler data with board
    /// snapshots from the registry.
    pub fn miner_telemetry(&self) -> MinerTelemetry {
        let mut telemetry = self.miner_telemetry_rx.borrow().clone();
        telemetry.boards = self
            .board_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .boards();
        telemetry
    }
}

/// Start the API server.
///
/// This function starts the HTTP API server and runs until the provided
/// cancellation token is triggered. It binds to localhost only by default for
/// security.
///
/// Board registrations arrive via `board_reg_rx` as boards connect. The
/// server manages the collection internally and cleans up when boards
/// disconnect.
pub async fn serve(
    config: ApiConfig,
    shutdown: CancellationToken,
    miner_telemetry_rx: watch::Receiver<MinerTelemetry>,
    mut board_reg_rx: mpsc::Receiver<BoardRegistration>,
    scheduler_cmd_tx: mpsc::Sender<SchedulerCommand>,
) -> Result<()> {
    let board_registry = Arc::new(Mutex::new(BoardRegistry::new()));

    // Drain board registrations into the registry as they arrive.
    // Exits when the sender is dropped (backplane shutdown).
    tokio::spawn({
        let registry = board_registry.clone();
        async move {
            while let Some(reg) = board_reg_rx.recv().await {
                registry.lock().unwrap_or_else(|e| e.into_inner()).push(reg);
            }
        }
    });

    let app = build_router(miner_telemetry_rx, board_registry, scheduler_cmd_tx);

    let listener = TcpListener::bind(&config.bind_addr).await?;
    let actual_addr = listener.local_addr()?;

    info!(url = %format!("http://{}", actual_addr), "API server listening.");

    // Warn if binding to non-localhost addresses
    if !actual_addr.ip().is_loopback() {
        warn!(
            "API server is bound to a non-localhost address ({}). \
             This exposes the API to the network without authentication.",
            actual_addr.ip()
        );
    }

    // Run server with graceful shutdown
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown.cancelled().await;
        })
        .await?;

    Ok(())
}

/// Build the application router with all API routes.
pub(crate) fn build_router(
    miner_telemetry_rx: watch::Receiver<MinerTelemetry>,
    board_registry: Arc<Mutex<BoardRegistry>>,
    scheduler_cmd_tx: mpsc::Sender<SchedulerCommand>,
) -> Router {
    let state = SharedState {
        miner_telemetry_rx,
        board_registry,
        scheduler_cmd_tx,
    };

    let (router, api) = OpenApiRouter::new()
        .nest("/api/v0", v0::routes())
        .with_state(state)
        .split_for_parts();

    router
        .route("/", routing::get(Redirect::permanent("/swagger-ui")))
        .route("/api", routing::get(Redirect::permanent("/swagger-ui")))
        .merge(SwaggerUi::new("/swagger-ui").url("/api/v0/openapi.json", api))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::TRACE))
                .on_response(DefaultOnResponse::new().level(Level::TRACE)),
        )
}

#[cfg(test)]
mod tests {
    use http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;
    use crate::api::commands::{BoardCommand, SchedulerCommand};
    use crate::api::registry::BoardRegistration;
    use crate::api_client::types::{
        AsicState, BoardTelemetry, Bzm2BusSummary, Bzm2ChainSummaryResponse,
        Bzm2ClockReportRequest, Bzm2ClockReportResponse, Bzm2DllClockStatus, Bzm2DtsVsQueryRequest,
        Bzm2EngineDiscoveryRequest, Bzm2LoopbackRequest, Bzm2LoopbackResponse, Bzm2NoopRequest,
        Bzm2NoopResponse, Bzm2PllClockStatus, Bzm2RegisterReadRequest, Bzm2RegisterReadResponse,
        Bzm2RegisterWriteRequest, Bzm2RegisterWriteResponse, Bzm2SavedOperatingPointStatus,
        Bzm2StartupPath, EngineCoordinate, SourceTelemetry, TemperatureSensor,
    };
    use crate::types::Temperature;

    /// Test fixtures returned by the router builder.
    struct TestFixtures {
        router: Router,
        /// Keep alive to prevent board watch channels from closing.
        _board_senders: Vec<watch::Sender<BoardTelemetry>>,
        /// Publish updated miner telemetry (e.g. after handling a command).
        _miner_tx: watch::Sender<MinerTelemetry>,
        /// Receives commands sent by PATCH handlers.
        _cmd_rx: mpsc::Receiver<SchedulerCommand>,
    }

    fn build_test_router(
        miner_state: MinerTelemetry,
        board_states: Vec<BoardTelemetry>,
    ) -> TestFixtures {
        let (miner_tx, miner_rx) = watch::channel(miner_state);
        let (cmd_tx, cmd_rx) = mpsc::channel::<SchedulerCommand>(16);

        let mut registry = BoardRegistry::new();
        let mut board_senders = Vec::new();
        for state in board_states {
            let (tx, rx) = watch::channel(state);
            registry.push(BoardRegistration {
                telemetry_rx: rx,
                command_tx: None,
            });
            board_senders.push(tx);
        }

        TestFixtures {
            router: build_router(miner_rx, Arc::new(Mutex::new(registry)), cmd_tx),
            _board_senders: board_senders,
            _miner_tx: miner_tx,
            _cmd_rx: cmd_rx,
        }
    }

    async fn get(app: Router, uri: &str) -> (http::StatusCode, String) {
        let req = Request::builder()
            .uri(uri)
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn health_returns_ok() {
        let fixtures = build_test_router(MinerTelemetry::default(), vec![]);
        let (status, body) = get(fixtures.router.clone(), "/api/v0/health").await;
        assert_eq!(status, 200);
        assert_eq!(body, "OK");
    }

    #[tokio::test]
    async fn miner_includes_boards_and_sources() {
        let miner_state = MinerTelemetry {
            uptime_secs: 42,
            hashrate: 1_000_000,
            shares_submitted: 5,
            sources: vec![SourceTelemetry {
                name: "pool".into(),
                url: Some("stratum+tcp://localhost:3333".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let board = BoardTelemetry {
            name: "test-board".into(),
            model: "TestModel".into(),
            ..Default::default()
        };
        let fixtures = build_test_router(miner_state, vec![board]);

        let (status, body) = get(fixtures.router.clone(), "/api/v0/miner").await;
        assert_eq!(status, 200);

        let state: MinerTelemetry = serde_json::from_str(&body).unwrap();
        assert_eq!(state.uptime_secs, 42);
        assert_eq!(state.hashrate, 1_000_000);
        assert_eq!(state.shares_submitted, 5);
        assert_eq!(state.boards.len(), 1);
        assert_eq!(state.boards[0].name, "test-board");
        assert_eq!(state.sources.len(), 1);
        assert_eq!(state.sources[0].name, "pool");
    }

    #[tokio::test]
    async fn boards_returns_list() {
        let boards = vec![
            BoardTelemetry {
                name: "board-a".into(),
                model: "A".into(),
                ..Default::default()
            },
            BoardTelemetry {
                name: "board-b".into(),
                model: "B".into(),
                ..Default::default()
            },
        ];
        let fixtures = build_test_router(MinerTelemetry::default(), boards);

        let (status, body) = get(fixtures.router.clone(), "/api/v0/boards").await;
        assert_eq!(status, 200);

        let boards: Vec<BoardTelemetry> = serde_json::from_str(&body).unwrap();
        assert_eq!(boards.len(), 2);
        assert_eq!(boards[0].name, "board-a");
        assert_eq!(boards[1].name, "board-b");
    }

    #[tokio::test]
    async fn board_by_name_returns_match() {
        let board = BoardTelemetry {
            name: "bitaxe-abc123".into(),
            model: "Bitaxe".into(),
            serial: Some("abc123".into()),
            ..Default::default()
        };
        let fixtures = build_test_router(MinerTelemetry::default(), vec![board]);

        let (status, body) = get(fixtures.router.clone(), "/api/v0/boards/bitaxe-abc123").await;
        assert_eq!(status, 200);

        let board: BoardTelemetry = serde_json::from_str(&body).unwrap();
        assert_eq!(board.name, "bitaxe-abc123");
        assert_eq!(board.serial, Some("abc123".into()));
    }

    #[tokio::test]
    async fn board_by_name_returns_404_when_missing() {
        let fixtures = build_test_router(MinerTelemetry::default(), vec![]);
        let (status, _body) = get(fixtures.router.clone(), "/api/v0/boards/nonexistent").await;
        assert_eq!(status, 404);
    }

    #[tokio::test]
    async fn sources_returns_list() {
        let miner_state = MinerTelemetry {
            sources: vec![
                SourceTelemetry {
                    name: "pool-a".into(),
                    url: Some("stratum+tcp://a:3333".into()),
                    ..Default::default()
                },
                SourceTelemetry {
                    name: "pool-b".into(),
                    url: None,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let fixtures = build_test_router(miner_state, vec![]);

        let (status, body) = get(fixtures.router.clone(), "/api/v0/sources").await;
        assert_eq!(status, 200);

        let sources: Vec<SourceTelemetry> = serde_json::from_str(&body).unwrap();
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].name, "pool-a");
        assert_eq!(sources[0].url.as_deref(), Some("stratum+tcp://a:3333"));
        assert_eq!(sources[1].name, "pool-b");
        assert_eq!(sources[1].url, None);
    }

    #[tokio::test]
    async fn source_by_name_returns_match() {
        let miner_state = MinerTelemetry {
            sources: vec![SourceTelemetry {
                name: "my-pool".into(),
                url: Some("stratum+tcp://pool:3333".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let fixtures = build_test_router(miner_state, vec![]);

        let (status, body) = get(fixtures.router.clone(), "/api/v0/sources/my-pool").await;
        assert_eq!(status, 200);

        let source: SourceTelemetry = serde_json::from_str(&body).unwrap();
        assert_eq!(source.name, "my-pool");
        assert_eq!(source.url.as_deref(), Some("stratum+tcp://pool:3333"));
    }

    #[tokio::test]
    async fn source_by_name_returns_404_when_missing() {
        let fixtures = build_test_router(MinerTelemetry::default(), vec![]);
        let (status, _body) = get(fixtures.router.clone(), "/api/v0/sources/nonexistent").await;
        assert_eq!(status, 404);
    }

    #[tokio::test]
    async fn source_difficulty_serializes_as_f64() {
        let miner_state = MinerTelemetry {
            sources: vec![SourceTelemetry {
                name: "pool".into(),
                difficulty: Some(2048.5),
                ..Default::default()
            }],
            ..Default::default()
        };
        let fixtures = build_test_router(miner_state, vec![]);

        let (status, body) = get(fixtures.router.clone(), "/api/v0/sources/pool").await;
        assert_eq!(status, 200);

        let source: SourceTelemetry = serde_json::from_str(&body).unwrap();
        assert_eq!(source.difficulty, Some(2048.5));
    }

    #[tokio::test]
    async fn unknown_route_returns_404() {
        let fixtures = build_test_router(MinerTelemetry::default(), vec![]);
        let (status, _body) = get(fixtures.router.clone(), "/api/v0/nope").await;
        assert_eq!(status, 404);
    }

    async fn post_json<T: serde::Serialize>(
        app: Router,
        method: &str,
        uri: &str,
        body: &T,
    ) -> (http::StatusCode, String) {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(serde_json::to_vec(body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn set_fan_target_round_trips_board_command() {
        use crate::api::commands::BoardCommand;
        use crate::api_client::types::{Fan, SetFanTargetRequest};

        let (miner_tx, miner_rx) = watch::channel(MinerTelemetry::default());
        let (cmd_tx, _cmd_rx) = mpsc::channel::<SchedulerCommand>(16);
        let mut registry = BoardRegistry::new();
        let (telemetry_tx, telemetry_rx) = watch::channel(BoardTelemetry {
            name: "fan-board".into(),
            model: "Test".into(),
            ..Default::default()
        });
        let (board_cmd_tx, mut board_cmd_rx) = mpsc::channel(1);
        registry.push(BoardRegistration {
            telemetry_rx,
            command_tx: Some(board_cmd_tx),
        });
        let router = build_router(miner_rx, Arc::new(Mutex::new(registry)), cmd_tx);

        // Clone for the task; the original must stay alive or the registry
        // prunes the board before the handler's post-command re-read.
        let telemetry_tx_for_command = telemetry_tx.clone();
        tokio::spawn(async move {
            if let Some(BoardCommand::SetFanTarget {
                board,
                fan,
                percent,
                reply,
            }) = board_cmd_rx.recv().await
            {
                assert_eq!(board, "fan-board");
                assert_eq!(fan, "fan0");
                assert_eq!(percent, Some(75));
                telemetry_tx_for_command.send_modify(|t| {
                    t.fans.push(Fan {
                        name: "fan0".into(),
                        rpm: None,
                        percent: None,
                        target_percent: percent,
                    });
                });
                let _ = reply.send(Ok(()));
            }
        });

        let (status, body) = post_json(
            router.clone(),
            "PATCH",
            "/api/v0/boards/fan-board/fans/fan0",
            &SetFanTargetRequest {
                target_percent: Some(75),
            },
        )
        .await;
        assert_eq!(status, 200);
        let board: BoardTelemetry = serde_json::from_str(&body).unwrap();
        assert_eq!(board.fans[0].target_percent, Some(75));

        // A board with no command channel answers 400.
        let (_keep, no_cmd_rx) = watch::channel(BoardTelemetry {
            name: "no-commands".into(),
            model: "Test".into(),
            ..Default::default()
        });
        let (miner_tx2, miner_rx2) = watch::channel(MinerTelemetry::default());
        let (cmd_tx2, _cmd_rx2) = mpsc::channel::<SchedulerCommand>(16);
        let mut registry2 = BoardRegistry::new();
        registry2.push(BoardRegistration {
            telemetry_rx: no_cmd_rx,
            command_tx: None,
        });
        let router2 = build_router(miner_rx2, Arc::new(Mutex::new(registry2)), cmd_tx2);
        let (status, _body) = post_json(
            router2,
            "PATCH",
            "/api/v0/boards/no-commands/fans/fan0",
            &SetFanTargetRequest {
                target_percent: Some(50),
            },
        )
        .await;
        assert_eq!(status, 400);

        drop(miner_tx);
        drop(miner_tx2);
        drop(telemetry_tx);
    }

    /// Build a router with one command-capable BZM2 test board, returning
    /// the board's telemetry sender and command receiver.
    fn build_bzm2_test_router(
        channel_capacity: usize,
    ) -> (
        Router,
        watch::Sender<BoardTelemetry>,
        mpsc::Receiver<BoardCommand>,
        watch::Sender<MinerTelemetry>,
        mpsc::Receiver<SchedulerCommand>,
    ) {
        let (miner_tx, miner_rx) = watch::channel(MinerTelemetry::default());
        let (cmd_tx, cmd_rx) = mpsc::channel::<SchedulerCommand>(16);
        let mut registry = BoardRegistry::new();
        let (telemetry_tx, telemetry_rx) = watch::channel(BoardTelemetry {
            name: "bzm2-test".into(),
            model: "BZM2".into(),
            ..Default::default()
        });
        let (board_cmd_tx, board_cmd_rx) = mpsc::channel(channel_capacity);
        registry.push(BoardRegistration {
            telemetry_rx,
            command_tx: Some(board_cmd_tx),
        });
        let router = build_router(miner_rx, Arc::new(Mutex::new(registry)), cmd_tx);
        (router, telemetry_tx, board_cmd_rx, miner_tx, cmd_rx)
    }

    #[tokio::test]
    async fn bzm2_query_endpoint_returns_refreshed_board_state() {
        let (router, telemetry_tx, mut board_cmd_rx, _miner_tx, _cmd_rx) =
            build_bzm2_test_router(1);

        // Clone for the task; the original must stay alive or the registry
        // prunes the board before the handler's post-command re-read.
        let telemetry_tx_for_command = telemetry_tx.clone();
        tokio::spawn(async move {
            if let Some(BoardCommand::QueryBzm2DtsVs {
                thread_index,
                asic,
                reply,
            }) = board_cmd_rx.recv().await
            {
                assert_eq!(thread_index, 0);
                assert_eq!(asic, 2);
                telemetry_tx_for_command.send_modify(|state| {
                    state.temperatures.push(TemperatureSensor {
                        name: "ttyUSB0-asic-2-dts".into(),
                        temperature: Some(Temperature::from_celsius(64.5)),
                    });
                });
                let _ = reply.send(Ok(()));
            }
        });

        let (status, body) = post_json(
            router,
            "POST",
            "/api/v0/boards/bzm2-test/bzm2/dts-vs-query",
            &Bzm2DtsVsQueryRequest {
                thread_index: 0,
                asic: 2,
            },
        )
        .await;

        assert_eq!(status, 200);
        let board: BoardTelemetry = serde_json::from_str(&body).unwrap();
        assert!(board.temperatures.iter().any(|sensor| {
            sensor.name == "ttyUSB0-asic-2-dts"
                && sensor.temperature.map(Temperature::as_degrees_c) == Some(64.5)
        }));
    }

    #[tokio::test]
    async fn bzm2_diagnostic_endpoints_round_trip_payloads() {
        let (router, _telemetry_tx, mut board_cmd_rx, _miner_tx, _cmd_rx) =
            build_bzm2_test_router(4);

        tokio::spawn(async move {
            while let Some(command) = board_cmd_rx.recv().await {
                match command {
                    BoardCommand::QueryBzm2Noop {
                        thread_index,
                        asic,
                        reply,
                    } => {
                        assert_eq!(thread_index, 0);
                        assert_eq!(asic, 2);
                        let _ = reply.send(Ok(*b"BZ2"));
                    }
                    BoardCommand::QueryBzm2Loopback {
                        thread_index,
                        asic,
                        payload,
                        reply,
                    } => {
                        assert_eq!(thread_index, 0);
                        assert_eq!(asic, 2);
                        assert_eq!(payload, vec![0x01, 0x02, 0xaa, 0xbb]);
                        let _ = reply.send(Ok(payload));
                    }
                    BoardCommand::ReadBzm2Register {
                        thread_index,
                        asic,
                        engine_address,
                        offset,
                        count,
                        reply,
                    } => {
                        assert_eq!(thread_index, 0);
                        assert_eq!(asic, 2);
                        assert_eq!(engine_address, 0x0fff);
                        assert_eq!(offset, 0x12);
                        assert_eq!(count, 4);
                        let _ = reply.send(Ok(vec![0x11, 0x22, 0x33, 0x44]));
                    }
                    BoardCommand::WriteBzm2Register {
                        thread_index,
                        asic,
                        engine_address,
                        offset,
                        value,
                        reply,
                    } => {
                        assert_eq!(thread_index, 0);
                        assert_eq!(asic, 2);
                        assert_eq!(engine_address, 0x0fff);
                        assert_eq!(offset, 0x12);
                        assert_eq!(value, vec![0xde, 0xad, 0xbe, 0xef]);
                        let _ = reply.send(Ok(()));
                    }
                    _ => {}
                }
            }
        });

        let (status, body) = post_json(
            router.clone(),
            "POST",
            "/api/v0/boards/bzm2-test/bzm2/noop",
            &Bzm2NoopRequest {
                thread_index: 0,
                asic: 2,
            },
        )
        .await;
        assert_eq!(status, 200);
        let noop: Bzm2NoopResponse = serde_json::from_str(&body).unwrap();
        assert_eq!(noop.payload_hex, "425a32");

        let (status, body) = post_json(
            router.clone(),
            "POST",
            "/api/v0/boards/bzm2-test/bzm2/loopback",
            &Bzm2LoopbackRequest {
                thread_index: 0,
                asic: 2,
                payload_hex: "0102aabb".into(),
            },
        )
        .await;
        assert_eq!(status, 200);
        let loopback: Bzm2LoopbackResponse = serde_json::from_str(&body).unwrap();
        assert_eq!(loopback.payload_hex, "0102aabb");

        let (status, body) = post_json(
            router.clone(),
            "POST",
            "/api/v0/boards/bzm2-test/bzm2/register-read",
            &Bzm2RegisterReadRequest {
                thread_index: 0,
                asic: 2,
                engine_address: 0x0fff,
                offset: 0x12,
                count: 4,
            },
        )
        .await;
        assert_eq!(status, 200);
        let readback: Bzm2RegisterReadResponse = serde_json::from_str(&body).unwrap();
        assert_eq!(readback.value_hex, "11223344");

        let (status, body) = post_json(
            router,
            "POST",
            "/api/v0/boards/bzm2-test/bzm2/register-write",
            &Bzm2RegisterWriteRequest {
                thread_index: 0,
                asic: 2,
                engine_address: 0x0fff,
                offset: 0x12,
                value_hex: "deadbeef".into(),
            },
        )
        .await;
        assert_eq!(status, 200);
        let write_ack: Bzm2RegisterWriteResponse = serde_json::from_str(&body).unwrap();
        assert_eq!(write_ack.bytes_written, 4);
    }

    #[tokio::test]
    async fn bzm2_chain_summary_endpoint_returns_live_layout() {
        let (router, _telemetry_tx, mut board_cmd_rx, _miner_tx, _cmd_rx) =
            build_bzm2_test_router(1);

        tokio::spawn(async move {
            if let Some(BoardCommand::QueryBzm2ChainSummary { reply }) = board_cmd_rx.recv().await {
                let _ = reply.send(Ok(Bzm2ChainSummaryResponse {
                    total_asics: 6,
                    startup_path: Some(Bzm2StartupPath::SavedReplay),
                    saved_operating_point_status: Some(Bzm2SavedOperatingPointStatus::Validated),
                    buses: vec![
                        Bzm2BusSummary {
                            thread_index: 0,
                            serial_path: "/dev/ttyUSB0".into(),
                            asic_start: 0,
                            asic_count: 2,
                        },
                        Bzm2BusSummary {
                            thread_index: 1,
                            serial_path: "/dev/ttyUSB1".into(),
                            asic_start: 2,
                            asic_count: 4,
                        },
                    ],
                }));
            }
        });

        let (status, body) = get(router, "/api/v0/boards/bzm2-test/bzm2/chain-summary").await;
        assert_eq!(status, 200);
        let summary: Bzm2ChainSummaryResponse = serde_json::from_str(&body).unwrap();
        assert_eq!(summary.total_asics, 6);
        assert_eq!(summary.startup_path, Some(Bzm2StartupPath::SavedReplay));
        assert_eq!(
            summary.saved_operating_point_status,
            Some(Bzm2SavedOperatingPointStatus::Validated)
        );
        assert_eq!(summary.buses.len(), 2);
        assert_eq!(summary.buses[1].serial_path, "/dev/ttyUSB1");
        assert_eq!(summary.buses[1].asic_start, 2);
        assert_eq!(summary.buses[1].asic_count, 4);
    }

    #[tokio::test]
    async fn bzm2_clock_report_endpoint_returns_payload() {
        let (router, _telemetry_tx, mut board_cmd_rx, _miner_tx, _cmd_rx) =
            build_bzm2_test_router(1);

        tokio::spawn(async move {
            if let Some(BoardCommand::QueryBzm2ClockReport {
                thread_index,
                asic,
                reply,
            }) = board_cmd_rx.recv().await
            {
                assert_eq!(thread_index, 0);
                assert_eq!(asic, 2);
                let _ = reply.send(Ok(Bzm2ClockReportResponse {
                    asic,
                    pll0: Bzm2PllClockStatus {
                        enable_register: 0x0000_0005,
                        misc_register: 0x0000_0012,
                        enabled: true,
                        locked: true,
                    },
                    pll1: Bzm2PllClockStatus {
                        enable_register: 0x0000_0001,
                        misc_register: 0x0000_001a,
                        enabled: true,
                        locked: false,
                    },
                    dll0: Bzm2DllClockStatus {
                        control2: 0x04,
                        control5: 0x07,
                        coarsecon: 0x03,
                        fincon: 0x9c,
                        freeze_valid: false,
                        locked: true,
                        fincon_valid: true,
                    },
                    dll1: Bzm2DllClockStatus {
                        control2: 0x06,
                        control5: 0x03,
                        coarsecon: 0x02,
                        fincon: 0x10,
                        freeze_valid: true,
                        locked: true,
                        fincon_valid: true,
                    },
                }));
            }
        });

        let (status, body) = post_json(
            router,
            "POST",
            "/api/v0/boards/bzm2-test/bzm2/clock-report",
            &Bzm2ClockReportRequest {
                thread_index: 0,
                asic: 2,
            },
        )
        .await;
        assert_eq!(status, 200);
        let report: Bzm2ClockReportResponse = serde_json::from_str(&body).unwrap();
        assert_eq!(report.asic, 2);
        assert_eq!(report.pll0.enable_register, 0x0000_0005);
        assert!(report.pll0.locked);
        assert!(!report.pll1.locked);
        assert_eq!(report.dll0.fincon, 0x9c);
        assert!(report.dll1.freeze_valid);
    }

    #[tokio::test]
    async fn bzm2_engine_discovery_endpoint_returns_refreshed_board_state() {
        let (router, telemetry_tx, mut board_cmd_rx, _miner_tx, _cmd_rx) =
            build_bzm2_test_router(1);

        // Clone for the task; the original must stay alive or the registry
        // prunes the board before the handler's post-command re-read.
        let telemetry_tx_for_command = telemetry_tx.clone();
        tokio::spawn(async move {
            if let Some(BoardCommand::DiscoverBzm2Engines {
                thread_index,
                asic,
                tdm_prediv_raw,
                tdm_counter,
                timeout_ms,
                reply,
            }) = board_cmd_rx.recv().await
            {
                assert_eq!(thread_index, 0);
                assert_eq!(asic, 2);
                assert_eq!(tdm_prediv_raw, 0x0f);
                assert_eq!(tdm_counter, 16);
                assert_eq!(timeout_ms, Some(150));
                telemetry_tx_for_command.send_modify(|state| {
                    state.asics.push(AsicState {
                        id: 2,
                        thread_index: Some(0),
                        serial_path: Some("/dev/ttyUSB0".into()),
                        discovered_engine_count: Some(236),
                        missing_engines: vec![
                            EngineCoordinate { row: 3, col: 7 },
                            EngineCoordinate { row: 5, col: 11 },
                        ],
                    });
                });
                let _ = reply.send(Ok(()));
            }
        });

        let (status, body) = post_json(
            router,
            "POST",
            "/api/v0/boards/bzm2-test/bzm2/discover-engines",
            &Bzm2EngineDiscoveryRequest {
                thread_index: 0,
                asic: 2,
                tdm_prediv_raw: 0x0f,
                tdm_counter: 16,
                timeout_ms: Some(150),
            },
        )
        .await;

        assert_eq!(status, 200);
        let board: BoardTelemetry = serde_json::from_str(&body).unwrap();
        assert!(board.asics.iter().any(|asic| {
            asic.id == 2
                && asic.thread_index == Some(0)
                && asic.discovered_engine_count == Some(236)
                && asic.missing_engines
                    == vec![
                        EngineCoordinate { row: 3, col: 7 },
                        EngineCoordinate { row: 5, col: 11 },
                    ]
        }));
    }
}
