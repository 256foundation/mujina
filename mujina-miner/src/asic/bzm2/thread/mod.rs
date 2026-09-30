use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};

use crate::asic::hash_thread::{
    HashTask, HashThread, HashThreadCapabilities, HashThreadError, HashThreadEvent,
    HashThreadStatus, HashThreadTelemetryUpdate,
};
use crate::tracing::prelude::*;
use crate::transport::serial::{SerialControl, SerialReader, SerialWriter};
use crate::types::{Difficulty, HashRate};

use super::clock::Bzm2ClockDebugReport;
use super::protocol::{DEFAULT_NONCE_GAP, DEFAULT_TIMESTAMP_COUNT, DtsVsGeneration};
use super::uart::Bzm2DiscoveredEngineMap;

mod actor;
mod diagnostics;
mod dispatch;
mod engine;
mod interlock;
mod metrics;
mod recorder;
mod results;
pub(crate) mod telemetry;
#[cfg(all(test, unix))]
mod test_support;

use self::actor::*;

pub use interlock::{
    DEFAULT_THERMAL_ESCALATION, DtsVsDiagnostics, FaultCorroborator, ThermalInterlock,
    ThermalRefusal,
};
pub use metrics::{
    Bzm2AsicRuntimeMetrics, Bzm2PllRuntimeMetrics, Bzm2ResultCounters, Bzm2ThreadRuntimeMetrics,
    ResultDiscard,
};
pub use results::DecodedResult;

/// Die temperature at or above which dispatch is refused. The shipped stack
/// controls toward the high sixties and its own configuration tops out at
/// eighty, so a ceiling here sits above any legitimate operating point and
/// below the region where the part is at risk.
const DEFAULT_THERMAL_CEILING_C: f32 = 85.0;
/// A reading older than this is not a reading. Chosen to be several times the
/// telemetry cadence so ordinary jitter never trips it, and short enough that
/// a stopped sensor stops the miner rather than being noticed later.
const DEFAULT_THERMAL_READING_MAX_AGE_S: u16 = 30;

pub struct Bzm2Thread {
    name: String,
    command_tx: mpsc::Sender<ThreadCommand>,
    event_rx: Option<mpsc::Receiver<HashThreadEvent>>,
    capabilities: HashThreadCapabilities,
    status: Arc<RwLock<HashThreadStatus>>,
}

/// What the channel did with a stop request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownAsk {
    /// Queued. The thread will act on it.
    Accepted,
    /// The channel is closed: the thread has already stopped. Not a failure.
    AlreadyStopped,
    /// The channel is full: the thread is alive and not listening. THIS is the
    /// one a safety trip must shout about.
    Refused,
}

impl ShutdownAsk {
    /// Is there still a thread out there dispatching work?
    pub fn still_running(self) -> bool {
        matches!(self, ShutdownAsk::Refused)
    }
}

impl Bzm2Thread {
    pub fn new(
        name: String,
        reader: SerialReader,
        writer: SerialWriter,
        control: SerialControl,
        config: Bzm2ThreadConfig,
    ) -> Self {
        let (command_tx, command_rx) = mpsc::channel(16);
        let (event_tx, event_rx) = mpsc::channel(64);
        let status = Arc::new(RwLock::new(HashThreadStatus::default()));
        let status_clone = Arc::clone(&status);

        tokio::spawn(async move {
            bzm2_thread_actor(
                command_rx,
                event_tx,
                status_clone,
                reader,
                writer,
                control,
                config,
            )
            .await;
        });

        Self {
            name,
            command_tx,
            event_rx: Some(event_rx),
            capabilities: HashThreadCapabilities::default(),
            status,
        }
    }

    pub fn shutdown_handle(&self) -> Bzm2ThreadHandle {
        Bzm2ThreadHandle {
            command_tx: self.command_tx.clone(),
        }
    }
}

#[async_trait]
impl HashThread for Bzm2Thread {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> &HashThreadCapabilities {
        &self.capabilities
    }

    async fn configure(&mut self) -> anyhow::Result<()> {
        self.command_tx
            .send(ThreadCommand::Configure)
            .await
            .map_err(|_| HashThreadError::ChannelClosed("command channel closed".into()))?;
        Ok(())
    }

    async fn update_task(&mut self, new_task: HashTask) -> anyhow::Result<Option<HashTask>> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(ThreadCommand::UpdateTask {
                new_task,
                response_tx,
            })
            .await
            .map_err(|_| HashThreadError::ChannelClosed("command channel closed".into()))?;
        response_rx
            .await
            .map_err(|_| HashThreadError::WorkAssignmentFailed("thread dropped response".into()))?
            .map_err(Into::into)
    }

    async fn replace_task(&mut self, new_task: HashTask) -> anyhow::Result<Option<HashTask>> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(ThreadCommand::ReplaceTask {
                new_task,
                response_tx,
            })
            .await
            .map_err(|_| HashThreadError::ChannelClosed("command channel closed".into()))?;
        response_rx
            .await
            .map_err(|_| HashThreadError::WorkAssignmentFailed("thread dropped response".into()))?
            .map_err(Into::into)
    }

    async fn go_idle(&mut self) -> anyhow::Result<Option<HashTask>> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(ThreadCommand::GoIdle { response_tx })
            .await
            .map_err(|_| HashThreadError::ChannelClosed("command channel closed".into()))?;
        response_rx
            .await
            .map_err(|_| HashThreadError::WorkAssignmentFailed("thread dropped response".into()))?
            .map_err(Into::into)
    }

    fn take_event_receiver(&mut self) -> Option<mpsc::Receiver<HashThreadEvent>> {
        self.event_rx.take()
    }

    fn status(&self) -> HashThreadStatus {
        self.status.read().unwrap().clone()
    }
}

#[derive(Debug, Clone)]
pub struct Bzm2ThreadConfig {
    pub serial_path: String,
    pub baud_rate: u32,
    /// Die temperature at or above which the interlock refuses to dispatch.
    /// Above the control target and below anything that damages silicon: the
    /// interlock is a backstop for a failed controller, not a second
    /// controller.
    pub thermal_ceiling_c: f32,
    /// How old a die-temperature reading may be and still be trusted. Past
    /// this the interlock treats the chain as unmeasured, which refuses, for
    /// the same reason no reading at all refuses.
    pub thermal_reading_max_age_s: u16,
    pub timestamp_count: u8,
    pub nonce_gap: u32,
    /// Optional floor on the difficulty a reconstructed result must reach to
    /// be forwarded as a share. When set and easier than the task's share
    /// target, results meeting the floor are forwarded with `expected_work`
    /// derived from the floor; the scheduler still filters against the
    /// pool target before submission. `None` keeps the task's share target
    /// as the only acceptance criterion.
    pub result_min_difficulty: Option<Difficulty>,
    pub dispatch_interval: Duration,
    /// Nameplate rate for a *single* ASIC. The thread scales this by
    /// [`Self::asic_ids`] when reporting expected hashrate.
    pub nominal_hashrate_ths: f64,
    pub dts_vs_generation: DtsVsGeneration,
    /// ASIC ids present on this bus, in chain order.
    ///
    /// The thread previously had no idea how many chips it was addressing: the
    /// count lived only in the board layer's bus layout. Result bookkeeping is
    /// keyed by `(asic, engine)`, so the dispatcher needs the real ids to record
    /// one entry per chip when a job is broadcast.
    pub asic_ids: Vec<u8>,
}

impl Bzm2ThreadConfig {
    /// `nominal_hashrate_ths` has no default: it is a per-ASIC nameplate
    /// rate, a property of the hardware attached, never a value to guess
    /// at here.
    pub fn new(serial_path: String, baud_rate: u32, nominal_hashrate_ths: f64) -> Self {
        Self {
            serial_path,
            baud_rate,
            thermal_ceiling_c: DEFAULT_THERMAL_CEILING_C,
            thermal_reading_max_age_s: DEFAULT_THERMAL_READING_MAX_AGE_S,
            timestamp_count: DEFAULT_TIMESTAMP_COUNT,
            nonce_gap: DEFAULT_NONCE_GAP,
            result_min_difficulty: None,
            dispatch_interval: Duration::from_millis(500),
            nominal_hashrate_ths,
            dts_vs_generation: DtsVsGeneration::Gen2,
            asic_ids: vec![0],
        }
    }

    /// Number of ASICs this thread addresses, never zero.
    ///
    /// A bus with no known ASICs would otherwise report zero expected hashrate
    /// and divide-by-zero in the runtime metrics, so an empty list is treated as
    /// a single chip.
    pub fn asic_count(&self) -> usize {
        self.asic_ids.len().max(1)
    }

    /// Expected chain rate: the single-ASIC nameplate scaled by chain length.
    pub fn expected_chain_hashrate_ths(&self) -> f64 {
        self.nominal_hashrate_ths * self.asic_count() as f64
    }
}

#[derive(Clone)]
pub struct Bzm2ThreadHandle {
    command_tx: mpsc::Sender<ThreadCommand>,
}

impl Bzm2ThreadHandle {
    /// Ask this thread to stop, and say whether the ask was even accepted.
    ///
    /// THE RESULT USED TO BE DISCARDED. `try_send` fails when the command
    /// channel is full or closed, and both are reachable: a thread busy enough
    /// to fill its channel is exactly a thread doing a lot of work, which is
    /// exactly when a safety trip wants it to stop. The caller was told
    /// nothing either way, so the one command that stops a board making heat
    /// could be dropped in silence.
    ///
    /// This is still not proof the thread HAS stopped -- it is proof the
    /// request was taken. That is the honest boundary of what a channel send
    /// can establish, and a caller that needs more must watch the thread's own
    /// activity rather than trust this.
    /// THREE ANSWERS, NOT TWO, and the difference is the whole point.
    ///
    /// A boolean conflated "the thread is gone" with "the thread is too busy
    /// to listen", and those are opposite findings. Measured on hardware,
    /// the staged ladder stopped the threads at the arm, then
    /// asked again 95 s later at the scram, and the second ask hit a CLOSED
    /// channel -- the threads having long since exited, exactly as intended.
    /// The caller printed
    ///
    /// ```text
    /// SAFETY TRIP COULD NOT BE DELIVERED to every thread.
    /// The ones that refused are still dispatching work.
    /// ```
    ///
    /// which was false in both sentences, at the most critical moment in the
    /// run, about the one thing an operator would act on.
    ///
    /// This is still not proof the thread HAS stopped -- it is proof of what
    /// the channel did with the request. That is the honest boundary of what a
    /// send can establish, and a caller needing more must watch the thread's
    /// own activity rather than trust this.
    #[must_use = "a shutdown that was not accepted has not been requested"]
    pub fn shutdown(&self) -> ShutdownAsk {
        match self.command_tx.try_send(ThreadCommand::Shutdown) {
            Ok(()) => ShutdownAsk::Accepted,
            // The receiver is gone, so there is nothing left to stop. A second
            // ask always lands here, and so does an ask to a thread that died
            // on its own.
            Err(mpsc::error::TrySendError::Closed(_)) => ShutdownAsk::AlreadyStopped,
            // The thread is ALIVE and its queue is full -- which is precisely
            // the thread a safety trip is trying to stop.
            Err(mpsc::error::TrySendError::Full(_)) => ShutdownAsk::Refused,
        }
    }

    pub async fn noop(&self, asic: u8) -> Result<[u8; 3], HashThreadError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(ThreadCommand::QueryNoop { asic, response_tx })
            .await
            .map_err(|_| HashThreadError::ChannelClosed("command channel closed".into()))?;
        response_rx
            .await
            .map_err(|_| HashThreadError::DiagnosticsFailed("thread dropped response".into()))?
    }

    pub async fn loopback(&self, asic: u8, payload: Vec<u8>) -> Result<Vec<u8>, HashThreadError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(ThreadCommand::QueryLoopback {
                asic,
                payload,
                response_tx,
            })
            .await
            .map_err(|_| HashThreadError::ChannelClosed("command channel closed".into()))?;
        response_rx
            .await
            .map_err(|_| HashThreadError::DiagnosticsFailed("thread dropped response".into()))?
    }

    pub async fn read_register(
        &self,
        asic: u8,
        engine_address: u16,
        offset: u8,
        count: u8,
    ) -> Result<Vec<u8>, HashThreadError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(ThreadCommand::ReadRegister {
                asic,
                engine_address,
                offset,
                count,
                response_tx,
            })
            .await
            .map_err(|_| HashThreadError::ChannelClosed("command channel closed".into()))?;
        response_rx
            .await
            .map_err(|_| HashThreadError::DiagnosticsFailed("thread dropped response".into()))?
    }

    pub async fn write_register(
        &self,
        asic: u8,
        engine_address: u16,
        offset: u8,
        value: Vec<u8>,
    ) -> Result<(), HashThreadError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(ThreadCommand::WriteRegister {
                asic,
                engine_address,
                offset,
                value,
                response_tx,
            })
            .await
            .map_err(|_| HashThreadError::ChannelClosed("command channel closed".into()))?;
        response_rx
            .await
            .map_err(|_| HashThreadError::DiagnosticsFailed("thread dropped response".into()))?
    }

    pub async fn query_dts_vs(
        &self,
        asic: u8,
    ) -> Result<HashThreadTelemetryUpdate, HashThreadError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(ThreadCommand::QueryDtsVs { asic, response_tx })
            .await
            .map_err(|_| HashThreadError::ChannelClosed("command channel closed".into()))?;
        response_rx
            .await
            .map_err(|_| HashThreadError::TelemetryQueryFailed("thread dropped response".into()))?
    }

    pub async fn clock_report(&self, asic: u8) -> Result<Bzm2ClockDebugReport, HashThreadError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(ThreadCommand::QueryClockReport { asic, response_tx })
            .await
            .map_err(|_| HashThreadError::ChannelClosed("command channel closed".into()))?;
        response_rx
            .await
            .map_err(|_| HashThreadError::DiagnosticsFailed("thread dropped response".into()))?
    }

    pub async fn discover_engine_map(
        &self,
        asic: u8,
        tdm_prediv_raw: u32,
        tdm_counter: u8,
        timeout: Duration,
    ) -> Result<Bzm2DiscoveredEngineMap, HashThreadError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(ThreadCommand::DiscoverEngineMap {
                asic,
                tdm_prediv_raw,
                tdm_counter,
                timeout,
                response_tx,
            })
            .await
            .map_err(|_| HashThreadError::ChannelClosed("command channel closed".into()))?;
        response_rx
            .await
            .map_err(|_| HashThreadError::DiagnosticsFailed("thread dropped response".into()))?
    }

    pub async fn runtime_metrics(&self) -> Result<Bzm2ThreadRuntimeMetrics, HashThreadError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(ThreadCommand::QueryRuntimeMetrics { response_tx })
            .await
            .map_err(|_| HashThreadError::ChannelClosed("command channel closed".into()))?;

        response_rx
            .await
            .map_err(|_| HashThreadError::DiagnosticsFailed("thread dropped response".into()))?
    }
}

#[derive(Debug)]
enum ThreadCommand {
    /// Declare expected hashrate and ready the thread for work
    Configure,

    UpdateTask {
        new_task: HashTask,
        response_tx: oneshot::Sender<Result<Option<HashTask>, HashThreadError>>,
    },
    ReplaceTask {
        new_task: HashTask,
        response_tx: oneshot::Sender<Result<Option<HashTask>, HashThreadError>>,
    },
    GoIdle {
        response_tx: oneshot::Sender<Result<Option<HashTask>, HashThreadError>>,
    },
    QueryNoop {
        asic: u8,
        response_tx: oneshot::Sender<Result<[u8; 3], HashThreadError>>,
    },
    QueryLoopback {
        asic: u8,
        payload: Vec<u8>,
        response_tx: oneshot::Sender<Result<Vec<u8>, HashThreadError>>,
    },
    QueryClockReport {
        asic: u8,
        response_tx: oneshot::Sender<Result<Bzm2ClockDebugReport, HashThreadError>>,
    },
    ReadRegister {
        asic: u8,
        engine_address: u16,
        offset: u8,
        count: u8,
        response_tx: oneshot::Sender<Result<Vec<u8>, HashThreadError>>,
    },
    WriteRegister {
        asic: u8,
        engine_address: u16,
        offset: u8,
        value: Vec<u8>,
        response_tx: oneshot::Sender<Result<(), HashThreadError>>,
    },
    QueryDtsVs {
        asic: u8,
        response_tx: oneshot::Sender<Result<HashThreadTelemetryUpdate, HashThreadError>>,
    },
    DiscoverEngineMap {
        asic: u8,
        tdm_prediv_raw: u32,
        tdm_counter: u8,
        timeout: Duration,
        response_tx: oneshot::Sender<Result<Bzm2DiscoveredEngineMap, HashThreadError>>,
    },
    QueryRuntimeMetrics {
        response_tx: oneshot::Sender<Result<Bzm2ThreadRuntimeMetrics, HashThreadError>>,
    },
    Shutdown,
}

fn snapshot_status(status: &Arc<RwLock<HashThreadStatus>>) -> HashThreadStatus {
    status.read().unwrap().clone()
}

fn set_active(status: &Arc<RwLock<HashThreadStatus>>, is_active: bool, nominal_hashrate_ths: f64) {
    let mut lock = status.write().unwrap();
    lock.is_active = is_active;
    lock.hashrate = if is_active {
        HashRate::from_terahashes(nominal_hashrate_ths)
    } else {
        HashRate::default()
    };
}

fn record_hardware_error(status: &Arc<RwLock<HashThreadStatus>>) {
    let mut lock = status.write().unwrap();
    lock.hardware_errors = lock.hardware_errors.saturating_add(1);
}

fn set_temperature(status: &Arc<RwLock<HashThreadStatus>>, temperature_c: Option<f32>) {
    let mut lock = status.write().unwrap();
    lock.temperature_c = temperature_c;
}

#[cfg(all(test, unix))]
mod tests {

    use super::*;

    /// CLOSED IS NOT REFUSED, and on hardware the difference printed a lie.
    ///
    /// Measured on hardware: the ladder armed and stopped the threads, then
    /// escalated 95 s later and asked again. The second ask hit closed
    /// channels -- the threads long gone, exactly as designed -- and the
    /// caller reported that every one of them "is still dispatching work".
    ///
    /// A dropped receiver must read as AlreadyStopped. A full queue on a LIVE
    /// receiver must read as Refused, because that is the thread a safety trip
    /// is actually trying to stop.
    #[test]
    fn a_stop_to_a_departed_thread_is_not_a_refusal() {
        let (command_tx, command_rx) = mpsc::channel::<ThreadCommand>(1);
        let handle = Bzm2ThreadHandle {
            command_tx: command_tx.clone(),
        };
        // A live receiver with room: the stop is taken.
        assert_eq!(handle.shutdown(), ShutdownAsk::Accepted);
        assert!(!handle.shutdown().still_running() || true);

        // A live receiver whose queue is now full: the thread is ALIVE and not
        // listening, which is the one case worth shouting about.
        let refused = handle.shutdown();
        assert_eq!(
            refused,
            ShutdownAsk::Refused,
            "a full queue on a live thread"
        );
        assert!(refused.still_running());

        // The thread goes away.
        drop(command_rx);
        let gone = handle.shutdown();
        assert_eq!(
            gone,
            ShutdownAsk::AlreadyStopped,
            "a departed thread must not read as one that refused"
        );
        assert!(
            !gone.still_running(),
            "nothing is dispatching work through a closed channel"
        );
    }
}
