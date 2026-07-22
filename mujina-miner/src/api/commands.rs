//! Command types sent from API handlers to backend components.
//!
//! Each command carries a oneshot reply channel so the handler can
//! await the result and translate it into an HTTP response.

use anyhow::Result;
use tokio::sync::oneshot;

use crate::api_client::types::{Bzm2ChainSummaryResponse, Bzm2ClockReportResponse};

/// Commands from the API to the scheduler.
pub enum SchedulerCommand {
    /// Pause job distribution to all threads.
    PauseMining { reply: oneshot::Sender<Result<()>> },

    /// Resume job distribution after a pause.
    ResumeMining { reply: oneshot::Sender<Result<()>> },
}

/// Commands from the API to board management.
pub enum BoardCommand {
    /// Set a fan's target duty cycle on a specific board.
    SetFanTarget {
        board: String,
        fan: String,
        /// Target duty cycle (0--100), or None for automatic control.
        percent: Option<u8>,
        reply: oneshot::Sender<Result<()>>,
    },

    /// Trigger a DTS/VS (temperature/voltage sensor) query on a BZM2
    /// ASIC; results are published into the board's telemetry stream.
    QueryBzm2DtsVs {
        thread_index: usize,
        asic: u8,
        reply: oneshot::Sender<Result<()>>,
    },

    /// Send a NOOP to a BZM2 ASIC and return the 3-byte payload
    /// (expected `b"BZ2"`).
    QueryBzm2Noop {
        thread_index: usize,
        asic: u8,
        reply: oneshot::Sender<Result<[u8; 3]>>,
    },

    /// Report the board's bus/ASIC layout and tuning status.
    QueryBzm2ChainSummary {
        reply: oneshot::Sender<Result<Bzm2ChainSummaryResponse>>,
    },

    /// Read PLL/DLL clock status registers from a BZM2 ASIC.
    QueryBzm2ClockReport {
        thread_index: usize,
        asic: u8,
        reply: oneshot::Sender<Result<Bzm2ClockReportResponse>>,
    },

    /// Echo a payload through a BZM2 ASIC's loopback path.
    QueryBzm2Loopback {
        thread_index: usize,
        asic: u8,
        payload: Vec<u8>,
        reply: oneshot::Sender<Result<Vec<u8>>>,
    },

    /// Read raw register bytes from a BZM2 engine address.
    ReadBzm2Register {
        thread_index: usize,
        asic: u8,
        engine_address: u16,
        offset: u8,
        count: u8,
        reply: oneshot::Sender<Result<Vec<u8>>>,
    },

    /// Write raw register bytes to a BZM2 engine address.
    WriteBzm2Register {
        thread_index: usize,
        asic: u8,
        engine_address: u16,
        offset: u8,
        value: Vec<u8>,
        reply: oneshot::Sender<Result<()>>,
    },

    /// Run TDM engine-map discovery on a BZM2 ASIC (idle threads
    /// only); results are published into the board's telemetry stream.
    DiscoverBzm2Engines {
        thread_index: usize,
        asic: u8,
        tdm_prediv_raw: u32,
        tdm_counter: u8,
        timeout_ms: Option<u32>,
        reply: oneshot::Sender<Result<()>>,
    },
}
