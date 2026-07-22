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

use super::commands::{BoardCommand, SchedulerCommand};
use super::server::SharedState;
use crate::api_client::types::{
    BoardTelemetry, MinerPatchRequest, MinerTelemetry, SetFanTargetRequest, SourceTelemetry,
};

/// Build the v0 API routes with OpenAPI metadata.
pub fn routes() -> OpenApiRouter<SharedState> {
    OpenApiRouter::new()
        .routes(routes!(health))
        .routes(routes!(get_miner, patch_miner))
        .routes(routes!(get_boards))
        .routes(routes!(get_board))
        .routes(routes!(set_fan_target))
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
        (status = OK, description = "Current miner telemetry", body = MinerTelemetry),
    ),
)]
async fn get_miner(State(state): State<SharedState>) -> Json<MinerTelemetry> {
    Json(state.miner_telemetry())
}

/// Apply partial updates to the miner configuration.
#[utoipa::path(
    patch,
    path = "/miner",
    tag = "miner",
    request_body = MinerPatchRequest,
    responses(
        (status = OK, description = "Updated miner telemetry", body = MinerTelemetry),
        (status = INTERNAL_SERVER_ERROR, description = "Command channel error"),
    ),
)]
async fn patch_miner(
    State(state): State<SharedState>,
    Json(req): Json<MinerPatchRequest>,
) -> Result<Json<MinerTelemetry>, StatusCode> {
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

    Ok(Json(state.miner_telemetry()))
}

/// Return all connected boards.
#[utoipa::path(
    get,
    path = "/boards",
    tag = "boards",
    responses(
        (status = OK, description = "List of connected boards", body = Vec<BoardTelemetry>),
    ),
)]
async fn get_boards(State(state): State<SharedState>) -> Json<Vec<BoardTelemetry>> {
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
        (status = OK, description = "Board details", body = BoardTelemetry),
        (status = NOT_FOUND, description = "Board not found"),
    ),
)]
async fn get_board(
    State(state): State<SharedState>,
    Path(name): Path<String>,
) -> Result<Json<BoardTelemetry>, StatusCode> {
    state
        .board_registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .board(&name)
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

/// Set a fan's target duty cycle on a board, or return it to automatic
/// control.
#[utoipa::path(
    patch,
    path = "/boards/{name}/fans/{fan}",
    tag = "boards",
    params(
        ("name" = String, Path, description = "Board name"),
        ("fan" = String, Path, description = "Fan name"),
    ),
    request_body = SetFanTargetRequest,
    responses(
        (status = OK, description = "Updated board telemetry", body = BoardTelemetry),
        (status = NOT_FOUND, description = "Board not found"),
        (status = BAD_REQUEST, description = "Board accepts no commands"),
        (status = INTERNAL_SERVER_ERROR, description = "Command channel error"),
    ),
)]
async fn set_fan_target(
    State(state): State<SharedState>,
    Path((name, fan)): Path<(String, String)>,
    Json(req): Json<SetFanTargetRequest>,
) -> Result<Json<BoardTelemetry>, StatusCode> {
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
        .send(BoardCommand::SetFanTarget {
            board: name.clone(),
            fan,
            percent: req.target_percent,
            reply: tx,
        })
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    // Result layers: timeout / channel-closed / command-error.
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
        (status = OK, description = "List of job sources", body = Vec<SourceTelemetry>),
    ),
)]
async fn get_sources(State(state): State<SharedState>) -> Json<Vec<SourceTelemetry>> {
    Json(state.miner_telemetry_rx.borrow().sources.clone())
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
        (status = OK, description = "Source details", body = SourceTelemetry),
        (status = NOT_FOUND, description = "Source not found"),
    ),
)]
async fn get_source(
    State(state): State<SharedState>,
    Path(name): Path<String>,
) -> Result<Json<SourceTelemetry>, StatusCode> {
    state
        .miner_telemetry_rx
        .borrow()
        .sources
        .iter()
        .find(|s| s.name == name)
        .cloned()
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}
