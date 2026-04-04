//! API v0 endpoints.
//!
//! Version 0 signals an unstable API -- breaking changes are expected
//! until the miner reaches 1.0.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use std::time::Duration;

use tokio::sync::oneshot;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::commands::SchedulerCommand;
use super::server::SharedState;
use crate::api_client::types::{
    BoardState, Bzm2DtsVsQueryRequest, Bzm2EngineDiscoveryRequest, Bzm2LoopbackRequest,
    Bzm2LoopbackResponse, Bzm2NoopRequest, Bzm2NoopResponse, Bzm2RegisterReadRequest,
    Bzm2RegisterReadResponse, Bzm2RegisterWriteRequest, Bzm2RegisterWriteResponse,
    MinerPatchRequest, MinerState, SourceState,
};
use crate::board::BoardCommand;

/// Build the v0 API routes with OpenAPI metadata.
pub fn routes() -> OpenApiRouter<SharedState> {
    OpenApiRouter::new()
        .routes(routes!(health))
        .routes(routes!(get_miner, patch_miner))
        .routes(routes!(get_boards))
        .routes(routes!(get_board))
        .routes(routes!(query_bzm2_dts_vs))
        .routes(routes!(query_bzm2_noop))
        .routes(routes!(query_bzm2_loopback))
        .routes(routes!(read_bzm2_register))
        .routes(routes!(write_bzm2_register))
        .routes(routes!(discover_bzm2_engines))
        .routes(routes!(get_sources))
        .routes(routes!(get_source))
}

/// Health check endpoint.
#[utoipa::path(
    get,
    path = "/health",
    tag = "health",
    responses(
        (status = OK, description = "Server is running", body = String),
    ),
)]
async fn health() -> &'static str {
    "OK"
}

/// Return the current miner state snapshot.
#[utoipa::path(
    get,
    path = "/miner",
    tag = "miner",
    responses(
        (status = OK, description = "Current miner state", body = MinerState),
    ),
)]
async fn get_miner(State(state): State<SharedState>) -> Json<MinerState> {
    Json(state.miner_state())
}

/// Apply partial updates to the miner configuration.
#[utoipa::path(
    patch,
    path = "/miner",
    tag = "miner",
    request_body = MinerPatchRequest,
    responses(
        (status = OK, description = "Updated miner state", body = MinerState),
        (status = INTERNAL_SERVER_ERROR, description = "Command channel error"),
    ),
)]
async fn patch_miner(
    State(state): State<SharedState>,
    Json(req): Json<MinerPatchRequest>,
) -> Result<Json<MinerState>, StatusCode> {
    if let Some(paused) = req.paused {
        let (tx, rx) = oneshot::channel();
        let cmd = if paused {
            SchedulerCommand::PauseMining { reply: tx }
        } else {
            SchedulerCommand::ResumeMining { reply: tx }
        };
        state
            .scheduler_cmd_tx
            .send(cmd)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        // Result layers: timeout / channel-closed / command-error.
        let Ok(Ok(Ok(()))) = tokio::time::timeout(Duration::from_secs(5), rx).await else {
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        };
    }

    Ok(Json(state.miner_state()))
}

/// Return all connected boards.
#[utoipa::path(
    get,
    path = "/boards",
    tag = "boards",
    responses(
        (status = OK, description = "List of connected boards", body = Vec<BoardState>),
    ),
)]
async fn get_boards(State(state): State<SharedState>) -> Json<Vec<BoardState>> {
    Json(
        state
            .board_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .boards(),
    )
}

/// Return a single board by name, or 404 if not found.
#[utoipa::path(
    get,
    path = "/boards/{name}",
    tag = "boards",
    params(
        ("name" = String, Path, description = "Board name"),
    ),
    responses(
        (status = OK, description = "Board details", body = BoardState),
        (status = NOT_FOUND, description = "Board not found"),
    ),
)]
async fn get_board(
    State(state): State<SharedState>,
    Path(name): Path<String>,
) -> Result<Json<BoardState>, StatusCode> {
    state
        .board_registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .boards()
        .into_iter()
        .find(|b| b.name == name)
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

/// Trigger an explicit BZM2 DTS/VS query and return the refreshed board state.
#[utoipa::path(
    post,
    path = "/boards/{name}/bzm2/dts-vs-query",
    tag = "boards",
    params(
        ("name" = String, Path, description = "Board name"),
    ),
    request_body = Bzm2DtsVsQueryRequest,
    responses(
        (status = OK, description = "Refreshed board details", body = BoardState),
        (status = BAD_REQUEST, description = "Board does not support BZM2 telemetry queries"),
        (status = NOT_FOUND, description = "Board not found"),
        (status = INTERNAL_SERVER_ERROR, description = "Board command failed"),
    ),
)]
async fn query_bzm2_dts_vs(
    State(state): State<SharedState>,
    Path(name): Path<String>,
    Json(req): Json<Bzm2DtsVsQueryRequest>,
) -> Result<Json<BoardState>, StatusCode> {
    let (board_exists, command_tx) = {
        let mut registry = state
            .board_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        (registry.board(&name).is_some(), registry.command_tx(&name))
    };
    if !board_exists {
        return Err(StatusCode::NOT_FOUND);
    }
    let Some(command_tx) = command_tx else {
        return Err(StatusCode::BAD_REQUEST);
    };

    let (tx, rx) = oneshot::channel();
    command_tx
        .send(BoardCommand::QueryBzm2DtsVs {
            thread_index: req.thread_index,
            asic: req.asic,
            reply: tx,
        })
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let Ok(Ok(Ok(()))) = tokio::time::timeout(Duration::from_secs(5), rx).await else {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    };

    state
        .board_registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .board(&name)
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

fn decode_hex_payload(raw: &str) -> Result<Vec<u8>, StatusCode> {
    hex::decode(raw.trim()).map_err(|_| StatusCode::BAD_REQUEST)
}

/// Trigger a live BZM2 NOOP diagnostic through a board-owned UART thread.
#[utoipa::path(
    post,
    path = "/boards/{name}/bzm2/noop",
    tag = "boards",
    params(
        ("name" = String, Path, description = "Board name"),
    ),
    request_body = Bzm2NoopRequest,
    responses(
        (status = OK, description = "NOOP response payload", body = Bzm2NoopResponse),
        (status = BAD_REQUEST, description = "Board does not support BZM2 diagnostics"),
        (status = NOT_FOUND, description = "Board not found"),
        (status = INTERNAL_SERVER_ERROR, description = "Board command failed"),
    ),
)]
async fn query_bzm2_noop(
    State(state): State<SharedState>,
    Path(name): Path<String>,
    Json(req): Json<Bzm2NoopRequest>,
) -> Result<Json<Bzm2NoopResponse>, StatusCode> {
    let (board_exists, command_tx) = {
        let mut registry = state
            .board_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        (registry.board(&name).is_some(), registry.command_tx(&name))
    };
    if !board_exists {
        return Err(StatusCode::NOT_FOUND);
    }
    let Some(command_tx) = command_tx else {
        return Err(StatusCode::BAD_REQUEST);
    };

    let (tx, rx) = oneshot::channel();
    command_tx
        .send(BoardCommand::QueryBzm2Noop {
            thread_index: req.thread_index,
            asic: req.asic,
            reply: tx,
        })
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let Ok(Ok(Ok(payload))) = tokio::time::timeout(Duration::from_secs(5), rx).await else {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    };

    Ok(Json(Bzm2NoopResponse {
        payload_hex: hex::encode(payload),
    }))
}

/// Trigger a live BZM2 loopback diagnostic through a board-owned UART thread.
#[utoipa::path(
    post,
    path = "/boards/{name}/bzm2/loopback",
    tag = "boards",
    params(
        ("name" = String, Path, description = "Board name"),
    ),
    request_body = Bzm2LoopbackRequest,
    responses(
        (status = OK, description = "Loopback response payload", body = Bzm2LoopbackResponse),
        (status = BAD_REQUEST, description = "Board does not support BZM2 diagnostics or request payload is invalid"),
        (status = NOT_FOUND, description = "Board not found"),
        (status = INTERNAL_SERVER_ERROR, description = "Board command failed"),
    ),
)]
async fn query_bzm2_loopback(
    State(state): State<SharedState>,
    Path(name): Path<String>,
    Json(req): Json<Bzm2LoopbackRequest>,
) -> Result<Json<Bzm2LoopbackResponse>, StatusCode> {
    let payload = decode_hex_payload(&req.payload_hex)?;
    let (board_exists, command_tx) = {
        let mut registry = state
            .board_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        (registry.board(&name).is_some(), registry.command_tx(&name))
    };
    if !board_exists {
        return Err(StatusCode::NOT_FOUND);
    }
    let Some(command_tx) = command_tx else {
        return Err(StatusCode::BAD_REQUEST);
    };

    let (tx, rx) = oneshot::channel();
    command_tx
        .send(BoardCommand::QueryBzm2Loopback {
            thread_index: req.thread_index,
            asic: req.asic,
            payload,
            reply: tx,
        })
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let Ok(Ok(Ok(payload))) = tokio::time::timeout(Duration::from_secs(5), rx).await else {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    };

    Ok(Json(Bzm2LoopbackResponse {
        payload_hex: hex::encode(payload),
    }))
}

/// Perform a live BZM2 register read through a board-owned UART thread.
#[utoipa::path(
    post,
    path = "/boards/{name}/bzm2/register-read",
    tag = "boards",
    params(
        ("name" = String, Path, description = "Board name"),
    ),
    request_body = Bzm2RegisterReadRequest,
    responses(
        (status = OK, description = "Register payload", body = Bzm2RegisterReadResponse),
        (status = BAD_REQUEST, description = "Board does not support BZM2 diagnostics"),
        (status = NOT_FOUND, description = "Board not found"),
        (status = INTERNAL_SERVER_ERROR, description = "Board command failed"),
    ),
)]
async fn read_bzm2_register(
    State(state): State<SharedState>,
    Path(name): Path<String>,
    Json(req): Json<Bzm2RegisterReadRequest>,
) -> Result<Json<Bzm2RegisterReadResponse>, StatusCode> {
    let (board_exists, command_tx) = {
        let mut registry = state
            .board_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        (registry.board(&name).is_some(), registry.command_tx(&name))
    };
    if !board_exists {
        return Err(StatusCode::NOT_FOUND);
    }
    let Some(command_tx) = command_tx else {
        return Err(StatusCode::BAD_REQUEST);
    };

    let (tx, rx) = oneshot::channel();
    command_tx
        .send(BoardCommand::ReadBzm2Register {
            thread_index: req.thread_index,
            asic: req.asic,
            engine_address: req.engine_address,
            offset: req.offset,
            count: req.count,
            reply: tx,
        })
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let Ok(Ok(Ok(value))) = tokio::time::timeout(Duration::from_secs(5), rx).await else {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    };

    Ok(Json(Bzm2RegisterReadResponse {
        value_hex: hex::encode(value),
    }))
}

/// Perform a live BZM2 register write through a board-owned UART thread.
#[utoipa::path(
    post,
    path = "/boards/{name}/bzm2/register-write",
    tag = "boards",
    params(
        ("name" = String, Path, description = "Board name"),
    ),
    request_body = Bzm2RegisterWriteRequest,
    responses(
        (status = OK, description = "Register write acknowledgement", body = Bzm2RegisterWriteResponse),
        (status = BAD_REQUEST, description = "Board does not support BZM2 diagnostics or request payload is invalid"),
        (status = NOT_FOUND, description = "Board not found"),
        (status = INTERNAL_SERVER_ERROR, description = "Board command failed"),
    ),
)]
async fn write_bzm2_register(
    State(state): State<SharedState>,
    Path(name): Path<String>,
    Json(req): Json<Bzm2RegisterWriteRequest>,
) -> Result<Json<Bzm2RegisterWriteResponse>, StatusCode> {
    let value = decode_hex_payload(&req.value_hex)?;
    let bytes_written = value.len();
    let (board_exists, command_tx) = {
        let mut registry = state
            .board_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        (registry.board(&name).is_some(), registry.command_tx(&name))
    };
    if !board_exists {
        return Err(StatusCode::NOT_FOUND);
    }
    let Some(command_tx) = command_tx else {
        return Err(StatusCode::BAD_REQUEST);
    };

    let (tx, rx) = oneshot::channel();
    command_tx
        .send(BoardCommand::WriteBzm2Register {
            thread_index: req.thread_index,
            asic: req.asic,
            engine_address: req.engine_address,
            offset: req.offset,
            value,
            reply: tx,
        })
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let Ok(Ok(Ok(()))) = tokio::time::timeout(Duration::from_secs(5), rx).await else {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    };

    Ok(Json(Bzm2RegisterWriteResponse { bytes_written }))
}

/// Trigger an explicit BZM2 engine-discovery scan and return the refreshed board state.
#[utoipa::path(
    post,
    path = "/boards/{name}/bzm2/discover-engines",
    tag = "boards",
    params(
        ("name" = String, Path, description = "Board name"),
    ),
    request_body = Bzm2EngineDiscoveryRequest,
    responses(
        (status = OK, description = "Refreshed board details", body = BoardState),
        (status = BAD_REQUEST, description = "Board does not support BZM2 engine discovery"),
        (status = NOT_FOUND, description = "Board not found"),
        (status = INTERNAL_SERVER_ERROR, description = "Board command failed"),
    ),
)]
async fn discover_bzm2_engines(
    State(state): State<SharedState>,
    Path(name): Path<String>,
    Json(req): Json<Bzm2EngineDiscoveryRequest>,
) -> Result<Json<BoardState>, StatusCode> {
    let (board_exists, command_tx) = {
        let mut registry = state
            .board_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        (registry.board(&name).is_some(), registry.command_tx(&name))
    };
    if !board_exists {
        return Err(StatusCode::NOT_FOUND);
    }
    let Some(command_tx) = command_tx else {
        return Err(StatusCode::BAD_REQUEST);
    };

    let (tx, rx) = oneshot::channel();
    command_tx
        .send(BoardCommand::DiscoverBzm2Engines {
            thread_index: req.thread_index,
            asic: req.asic,
            tdm_prediv_raw: req.tdm_prediv_raw,
            tdm_counter: req.tdm_counter,
            timeout_ms: req.timeout_ms,
            reply: tx,
        })
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let Ok(Ok(Ok(()))) = tokio::time::timeout(Duration::from_secs(5), rx).await else {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    };

    state
        .board_registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .board(&name)
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

/// Return all registered job sources.
#[utoipa::path(
    get,
    path = "/sources",
    tag = "sources",
    responses(
        (status = OK, description = "List of job sources", body = Vec<SourceState>),
    ),
)]
async fn get_sources(State(state): State<SharedState>) -> Json<Vec<SourceState>> {
    Json(state.miner_state().sources)
}

/// Return a single source by name, or 404 if not found.
#[utoipa::path(
    get,
    path = "/sources/{name}",
    tag = "sources",
    params(
        ("name" = String, Path, description = "Source name"),
    ),
    responses(
        (status = OK, description = "Source details", body = SourceState),
        (status = NOT_FOUND, description = "Source not found"),
    ),
)]
async fn get_source(
    State(state): State<SharedState>,
    Path(name): Path<String>,
) -> Result<Json<SourceState>, StatusCode> {
    state
        .miner_state()
        .sources
        .into_iter()
        .find(|s| s.name == name)
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}
