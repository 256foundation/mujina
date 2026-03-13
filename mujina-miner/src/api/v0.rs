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
    BoardPatchRequest, BoardState, MinerPatchRequest, MinerState, SourceState,
};

/// Build the v0 API routes with OpenAPI metadata.
pub fn routes() -> OpenApiRouter<SharedState> {
    OpenApiRouter::new()
        .routes(routes!(health))
        .routes(routes!(get_miner, patch_miner))
        .routes(routes!(get_boards))
        .routes(routes!(get_board, patch_board))
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

/// Apply partial updates to a board's configuration.
#[utoipa::path(
    patch,
    path = "/boards/{name}",
    tag = "boards",
    params(
        ("name" = String, Path, description = "Board name"),
    ),
    request_body = BoardPatchRequest,
    responses(
        (status = OK, description = "Updated board state", body = BoardState),
        (status = NOT_FOUND, description = "Board not found"),
        (status = UNPROCESSABLE_ENTITY, description = "Unknown power domain or voltage out of range"),
        (status = INTERNAL_SERVER_ERROR, description = "Command channel error or hardware fault"),
    ),
)]
async fn patch_board(
    State(state): State<SharedState>,
    Path(name): Path<String>,
    Json(req): Json<BoardPatchRequest>,
) -> Result<Json<BoardState>, StatusCode> {
    if let Some(powers) = req.powers {
        // Resolve the board's command sender while holding the lock as briefly as possible.
        let (board_exists, cmd_tx) = {
            let registry = state
                .board_registry
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            (registry.contains(&name), registry.find_cmd_tx(&name))
        };

        if !board_exists {
            return Err(StatusCode::NOT_FOUND);
        }
        let cmd_tx = cmd_tx.ok_or(StatusCode::UNPROCESSABLE_ENTITY)?;

        for patch in powers {
            let Some(voltage_v) = patch.voltage_v else {
                continue;
            };
            let (reply_tx, reply_rx) = oneshot::channel();
            cmd_tx
                .send(BoardCommand::SetVoltage {
                    domain: patch.name,
                    voltage_v,
                    reply: reply_tx,
                })
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

            let result = tokio::time::timeout(Duration::from_secs(5), reply_rx).await;
            match result {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(_))) => return Err(StatusCode::UNPROCESSABLE_ENTITY),
                _ => return Err(StatusCode::INTERNAL_SERVER_ERROR),
            }
        }
    }

    // Return the current board snapshot.
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
