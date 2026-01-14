//! BM13xx HashThread implementation.
//!
//! This module provides the HashThread implementation for BM13xx family ASIC
//! chips (BM1362, BM1366, BM1370, etc.). A BM13xxThread represents a chain of
//! BM13xx chips connected via a shared serial bus.
//!
//! The thread is implemented as an actor task that monitors the serial bus for
//! chip responses, filters shares, and manages work assignment.

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use bitcoin::block::Header as BlockHeader;
use futures::{sink::Sink, stream::Stream, SinkExt};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_stream::StreamExt;

use super::protocol;
use crate::{
    asic::hash_thread::{
        BoardPeripherals, HashTask, HashThread, HashThreadCapabilities, HashThreadError,
        HashThreadEvent, HashThreadStatus, Share, ThreadRemovalSignal,
    },
    tracing::prelude::*,
    types::{Difficulty, HashRate},
    u256::U256,
};

/// Tracks tasks sent to chip hardware, indexed by chip_job_id.
///
/// BM13xx chips use 4-bit job IDs. This tracker maintains snapshots of
/// HashTasks sent to the chip so we can match nonce responses back to the
/// correct task context (EN2, ntime, etc.).
struct ChipJobTracker {
    tasks: [Option<HashTask>; 16],
    next_id: u8,
}

impl ChipJobTracker {
    fn new() -> Self {
        Self {
            tasks: Default::default(),
            next_id: 0,
        }
    }

    fn insert(&mut self, task: HashTask) -> u8 {
        let chip_job_id = self.next_id;
        self.tasks[chip_job_id as usize] = Some(task);
        self.next_id = (self.next_id + 1) % (self.tasks.len() as u8);
        chip_job_id
    }

    fn get(&self, chip_job_id: u8) -> Option<&HashTask> {
        self.tasks
            .get(chip_job_id as usize)
            .and_then(|t| t.as_ref())
    }

    fn clear(&mut self) {
        self.tasks = Default::default();
    }
}

/// Command messages sent from scheduler to thread
#[derive(Debug)]
enum ThreadCommand {
    /// Update task (old shares still valid)
    UpdateTask {
        new_task: HashTask,
        response_tx: oneshot::Sender<std::result::Result<Option<HashTask>, HashThreadError>>,
    },

    /// Replace task (old shares invalid)
    ReplaceTask {
        new_task: HashTask,
        response_tx: oneshot::Sender<std::result::Result<Option<HashTask>, HashThreadError>>,
    },

    /// Go idle (stop hashing, low power)
    GoIdle {
        response_tx: oneshot::Sender<std::result::Result<Option<HashTask>, HashThreadError>>,
    },

    /// Shutdown the thread
    #[expect(unused)]
    Shutdown,
}

/// BM13xx HashThread implementation.
///
/// Represents a chain of BM13xx chips as a schedulable worker. The thread
/// manages serial communication with chips, filters shares, and reports events.
/// Chip initialization happens lazily when first work is assigned.
pub struct BM13xxThread {
    /// Human-readable name for logging
    name: String,

    /// Channel for sending commands to the actor
    command_tx: mpsc::Sender<ThreadCommand>,

    /// Event receiver (taken by scheduler)
    event_rx: Option<mpsc::Receiver<HashThreadEvent>>,

    /// Cached capabilities
    capabilities: HashThreadCapabilities,

    /// Shared status (updated by actor task)
    status: Arc<RwLock<HashThreadStatus>>,
}

impl BM13xxThread {
    /// Create a new BM13xx thread with Stream/Sink for chip communication
    ///
    /// Thread starts with chip disabled. Chip will be initialized when first
    /// work is assigned.
    ///
    /// # Arguments
    /// * `name` - Human-readable name for logging (e.g., "Bitaxe Gamma (e2f56f9b)")
    /// * `chip_responses` - Stream of decoded responses from chips
    /// * `chip_commands` - Sink for sending encoded commands to chips
    /// * `peripherals` - Hardware interfaces from board (enable, regulator, etc.)
    /// * `removal_rx` - Watch channel for board-triggered removal
    pub fn new<R, W>(
        name: String,
        chip_responses: R,
        chip_commands: W,
        peripherals: BoardPeripherals,
        removal_rx: watch::Receiver<ThreadRemovalSignal>,
    ) -> Self
    where
        R: Stream<Item = Result<protocol::Response, std::io::Error>> + Unpin + Send + 'static,
        W: Sink<protocol::Command> + Unpin + Send + 'static,
        W::Error: std::fmt::Debug,
    {
        let (cmd_tx, cmd_rx) = mpsc::channel(10);
        let (evt_tx, evt_rx) = mpsc::channel(100);

        let status = Arc::new(RwLock::new(HashThreadStatus::default()));
        let status_clone = Arc::clone(&status);

        // Spawn the actor task
        tokio::spawn(async move {
            bm13xx_thread_actor(
                cmd_rx,
                evt_tx,
                removal_rx,
                status_clone,
                chip_responses,
                chip_commands,
                peripherals,
            )
            .await;
        });

        Self {
            name,
            command_tx: cmd_tx,
            event_rx: Some(evt_rx),
            capabilities: HashThreadCapabilities {
                hashrate_estimate: HashRate::from_terahashes(1.0), // Stub
            },
            status,
        }
    }
}

#[async_trait]
impl HashThread for BM13xxThread {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> &HashThreadCapabilities {
        &self.capabilities
    }

    async fn update_task(
        &mut self,
        new_task: HashTask,
    ) -> std::result::Result<Option<HashTask>, HashThreadError> {
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
            .map_err(|_| HashThreadError::WorkAssignmentFailed("no response from thread".into()))?
    }

    async fn replace_task(
        &mut self,
        new_task: HashTask,
    ) -> std::result::Result<Option<HashTask>, HashThreadError> {
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
            .map_err(|_| HashThreadError::WorkAssignmentFailed("no response from thread".into()))?
    }

    async fn go_idle(&mut self) -> std::result::Result<Option<HashTask>, HashThreadError> {
        let (response_tx, response_rx) = oneshot::channel();

        self.command_tx
            .send(ThreadCommand::GoIdle { response_tx })
            .await
            .map_err(|_| HashThreadError::ChannelClosed("command channel closed".into()))?;

        response_rx
            .await
            .map_err(|_| HashThreadError::WorkAssignmentFailed("no response from thread".into()))?
    }

    fn take_event_receiver(&mut self) -> Option<mpsc::Receiver<HashThreadEvent>> {
        self.event_rx.take()
    }

    fn status(&self) -> HashThreadStatus {
        self.status.read().unwrap().clone()
    }
}

/// Initialize BM13xx chip for mining.
///
/// Enables chip, configures all registers, and ramps frequency to target.
async fn initialize_chip<W>(
    chip_commands: &mut W,
    peripherals: &mut BoardPeripherals,
) -> Result<(), HashThreadError>
where
    W: Sink<protocol::Command> + Unpin,
    W::Error: std::fmt::Debug,
{
    use protocol::{Command, Register};

    // Enable the ASIC
    if let Some(ref mut asic_enable) = peripherals.asic_enable {
        debug!("Enabling ASIC");
        asic_enable.enable().await.map_err(|e| {
            HashThreadError::InitializationFailed(format!("Failed to enable ASIC: {}", e))
        })?;
    }

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Send version mask configuration (3 times)
    debug!("Configuring version mask");
    for _ in 1..=3 {
        chip_commands
            .send(Command::WriteRegister {
                broadcast: true,
                chip_address: 0x00,
                register: Register::VersionMask(protocol::VersionMask::full_rolling()),
            })
            .await
            .map_err(|e| {
                HashThreadError::InitializationFailed(format!(
                    "Failed to send version mask: {:?}",
                    e
                ))
            })?;
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    // Pre-configuration registers
    debug!("Sending pre-configuration registers");

    chip_commands
        .send(Command::WriteRegister {
            broadcast: true,
            chip_address: 0x00,
            register: Register::InitControl {
                raw_value: 0x00000700,
            },
        })
        .await
        .map_err(|e| HashThreadError::InitializationFailed(format!("Init send failed: {:?}", e)))?;

    chip_commands
        .send(Command::WriteRegister {
            broadcast: true,
            chip_address: 0x00,
            register: Register::MiscControl {
                raw_value: 0x00C100F0,
            },
        })
        .await
        .map_err(|e| HashThreadError::InitializationFailed(format!("Misc send failed: {:?}", e)))?;

    chip_commands
        .send(Command::ChainInactive)
        .await
        .map_err(|e| {
            HashThreadError::InitializationFailed(format!("ChainInactive failed: {:?}", e))
        })?;

    chip_commands
        .send(Command::SetChipAddress { chip_address: 0x00 })
        .await
        .map_err(|e| {
            HashThreadError::InitializationFailed(format!("SetChipAddress failed: {:?}", e))
        })?;

    // Core configuration (broadcast)
    debug!("Sending broadcast core configuration");

    chip_commands
        .send(Command::WriteRegister {
            broadcast: true,
            chip_address: 0x00,
            register: Register::Core {
                raw_value: 0x8000_8B00,
            },
        })
        .await
        .map_err(|e| {
            HashThreadError::InitializationFailed(format!("Core1 send failed: {:?}", e))
        })?;

    chip_commands
        .send(Command::WriteRegister {
            broadcast: true,
            chip_address: 0x00,
            register: Register::Core {
                raw_value: 0x8000_800C,
            },
        })
        .await
        .map_err(|e| {
            HashThreadError::InitializationFailed(format!("Core2 send failed: {:?}", e))
        })?;

    // Ticket mask, IO strength
    // Target: ~1 nonce per second at 1 TH/s (1000 GiH/s = 1.074 TH/s)
    use protocol::{Hashrate, ReportingInterval, ReportingRate, TicketMask};
    let reporting_interval = ReportingInterval::from_rate(
        Hashrate::gibihashes_per_sec(1000.0),
        ReportingRate::nonces_per_sec(1.0),
    );
    let ticket_mask = TicketMask::new(reporting_interval);

    chip_commands
        .send(Command::WriteRegister {
            broadcast: true,
            chip_address: 0x00,
            register: Register::TicketMask(ticket_mask),
        })
        .await
        .map_err(|e| {
            HashThreadError::InitializationFailed(format!("TicketMask send failed: {:?}", e))
        })?;

    chip_commands
        .send(Command::WriteRegister {
            broadcast: true,
            chip_address: 0x00,
            register: Register::IoDriverStrength(protocol::IoDriverStrength::normal()),
        })
        .await
        .map_err(|e| {
            HashThreadError::InitializationFailed(format!("IoDriver send failed: {:?}", e))
        })?;

    // Chip-specific configuration
    debug!("Sending chip-specific configuration");

    chip_commands
        .send(Command::WriteRegister {
            broadcast: false,
            chip_address: 0x00,
            register: Register::InitControl {
                raw_value: 0xF0010700,
            },
        })
        .await
        .map_err(|e| {
            HashThreadError::InitializationFailed(format!("InitControl chip send failed: {:?}", e))
        })?;

    chip_commands
        .send(Command::WriteRegister {
            broadcast: false,
            chip_address: 0x00,
            register: Register::MiscControl {
                raw_value: 0x00C100F0,
            },
        })
        .await
        .map_err(|e| {
            HashThreadError::InitializationFailed(format!("MiscControl chip send failed: {:?}", e))
        })?;

    chip_commands
        .send(Command::WriteRegister {
            broadcast: false,
            chip_address: 0x00,
            register: Register::Core {
                raw_value: 0x8000_8B00,
            },
        })
        .await
        .map_err(|e| {
            HashThreadError::InitializationFailed(format!("Core chip1 send failed: {:?}", e))
        })?;

    chip_commands
        .send(Command::WriteRegister {
            broadcast: false,
            chip_address: 0x00,
            register: Register::Core {
                raw_value: 0x8000_800C,
            },
        })
        .await
        .map_err(|e| {
            HashThreadError::InitializationFailed(format!("Core chip2 send failed: {:?}", e))
        })?;

    chip_commands
        .send(Command::WriteRegister {
            broadcast: false,
            chip_address: 0x00,
            register: Register::Core {
                raw_value: 0x8000_82AA,
            },
        })
        .await
        .map_err(|e| {
            HashThreadError::InitializationFailed(format!("Core chip3 send failed: {:?}", e))
        })?;

    // Additional settings
    chip_commands
        .send(Command::WriteRegister {
            broadcast: true,
            chip_address: 0x00,
            register: Register::MiscSettings {
                raw_value: 0x80440000,
            },
        })
        .await
        .map_err(|e| {
            HashThreadError::InitializationFailed(format!("MiscSettings send failed: {:?}", e))
        })?;

    chip_commands
        .send(Command::WriteRegister {
            broadcast: true,
            chip_address: 0x00,
            register: Register::AnalogMux {
                raw_value: 0x02000000,
            },
        })
        .await
        .map_err(|e| {
            HashThreadError::InitializationFailed(format!("AnalogMux send failed: {:?}", e))
        })?;

    chip_commands
        .send(Command::WriteRegister {
            broadcast: true,
            chip_address: 0x00,
            register: Register::MiscSettings {
                raw_value: 0x80440000,
            },
        })
        .await
        .map_err(|e| {
            HashThreadError::InitializationFailed(format!("MiscSettings2 send failed: {:?}", e))
        })?;

    chip_commands
        .send(Command::WriteRegister {
            broadcast: true,
            chip_address: 0x00,
            register: Register::Core {
                raw_value: 0x8000_8DEE,
            },
        })
        .await
        .map_err(|e| {
            HashThreadError::InitializationFailed(format!("Core final send failed: {:?}", e))
        })?;

    // Frequency ramping (56.25 MHz -> 525 MHz)
    debug!("Ramping frequency from 56.25 MHz to 525 MHz");
    let frequency_steps = generate_frequency_ramp_steps(56.25, 525.0, 6.25);

    for (i, pll_config) in frequency_steps.iter().enumerate() {
        chip_commands
            .send(Command::WriteRegister {
                broadcast: true,
                chip_address: 0x00,
                register: Register::PllDivider(*pll_config),
            })
            .await
            .map_err(|e| {
                HashThreadError::InitializationFailed(format!("PLL ramp failed: {:?}", e))
            })?;

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        if i % 10 == 0 || i == frequency_steps.len() - 1 {
            trace!("Frequency ramp step {}/{}", i + 1, frequency_steps.len());
        }
    }

    debug!("Frequency ramping complete");

    // Final configuration
    chip_commands
        .send(Command::WriteRegister {
            broadcast: true,
            chip_address: 0x00,
            register: Register::NonceRange(protocol::NonceRangeConfig::from_raw(0xB51E0000)),
        })
        .await
        .map_err(|e| {
            HashThreadError::InitializationFailed(format!("NonceRange send failed: {:?}", e))
        })?;

    // Final version mask
    chip_commands
        .send(Command::WriteRegister {
            broadcast: true,
            chip_address: 0x00,
            register: Register::VersionMask(protocol::VersionMask::full_rolling()),
        })
        .await
        .map_err(|e| {
            HashThreadError::InitializationFailed(format!("Final version mask failed: {:?}", e))
        })?;

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    Ok(())
}

/// Generate frequency ramp steps for smooth PLL transitions
fn generate_frequency_ramp_steps(
    start_mhz: f32,
    target_mhz: f32,
    step_mhz: f32,
) -> Vec<protocol::PllConfig> {
    let mut configs = Vec::new();
    let mut current = start_mhz;

    while current <= target_mhz {
        if let Some(config) = calculate_pll_for_frequency(current) {
            configs.push(config);
        }
        current += step_mhz;
        if current > target_mhz && (current - step_mhz) < target_mhz {
            current = target_mhz;
        }
    }

    configs
}

/// Convert HashTask to JobFullFormat for chip hardware.
///
/// Extracts or computes the merkle root, then builds a JobFullFormat with all
/// block header fields. For computed merkle roots, requires EN2. For fixed merkle
/// roots (Stratum v2 header-only), uses the template's fixed value directly.
fn task_to_job_full(
    task: &HashTask,
    chip_job_id: u8,
) -> Result<protocol::JobFullFormat, HashThreadError> {
    use crate::job_source::MerkleRootKind;

    let template = task.template.as_ref();

    // Get merkle root (computed or fixed)
    let merkle_root = match &template.merkle_root {
        MerkleRootKind::Computed(_) => {
            // Extract EN2 (required for computed merkle roots)
            let en2 = task.en2.as_ref().ok_or_else(|| {
                HashThreadError::WorkAssignmentFailed(
                    "EN2 required for computed merkle root".into(),
                )
            })?;

            // Compute merkle root for this EN2
            template.compute_merkle_root(en2).map_err(|e| {
                HashThreadError::WorkAssignmentFailed(format!(
                    "Merkle root computation failed: {}",
                    e
                ))
            })?
        }
        MerkleRootKind::Fixed(merkle_root) => *merkle_root,
    };

    Ok(protocol::JobFullFormat {
        job_id: chip_job_id,
        num_midstates: 1,
        starting_nonce: 0,
        nbits: template.bits,
        ntime: task.ntime,
        merkle_root,
        prev_block_hash: template.prev_blockhash,
        version: template.version.base(),
    })
}

/// Calculate PLL configuration for a specific frequency
fn calculate_pll_for_frequency(target_freq: f32) -> Option<protocol::PllConfig> {
    const CRYSTAL_FREQ: f32 = 25.0;
    const MAX_FREQ_ERROR: f32 = 1.0;

    let mut best_fb_div = 0u8;
    let mut best_ref_div = 0u8;
    let mut best_post_div1 = 0u8;
    let mut best_post_div2 = 0u8;
    let mut min_error = 10.0;

    for ref_div in [2, 1] {
        if best_fb_div != 0 {
            break;
        }
        for post_div1 in (1..=7).rev() {
            if best_fb_div != 0 {
                break;
            }
            for post_div2 in (1..=7).rev() {
                if best_fb_div != 0 {
                    break;
                }
                if post_div1 >= post_div2 {
                    let fb_div_f = (post_div1 * post_div2) as f32 * target_freq * ref_div as f32
                        / CRYSTAL_FREQ;
                    let fb_div = fb_div_f.round() as u8;

                    if (0xa0..=0xef).contains(&fb_div) {
                        let actual_freq =
                            CRYSTAL_FREQ * fb_div as f32 / (ref_div * post_div1 * post_div2) as f32;
                        let error = (actual_freq - target_freq).abs();

                        if error < min_error && error < MAX_FREQ_ERROR {
                            best_fb_div = fb_div;
                            best_ref_div = ref_div;
                            best_post_div1 = post_div1;
                            best_post_div2 = post_div2;
                            min_error = error;
                        }
                    }
                }
            }
        }
    }

    if best_fb_div == 0 {
        return None;
    }

    let post_div = ((best_post_div1 - 1) << 4) | (best_post_div2 - 1);
    Some(protocol::PllConfig::new(
        best_fb_div,
        best_ref_div,
        post_div,
    ))
}

/// Internal actor task for BM13xxThread.
///
/// This runs as an independent Tokio task and handles:
/// - Commands from scheduler (update/replace work, go idle, shutdown)
/// - Removal signal from board (USB unplug, fault, etc.)
/// - Chip initialization (lazy, on first work assignment)
/// - Serial communication with chips
/// - Share filtering and event emission (TODO)
///
/// Chip is disabled on startup to establish known state. Chip is enabled and
/// configured when scheduler assigns first work.
async fn bm13xx_thread_actor<R, W>(
    mut cmd_rx: mpsc::Receiver<ThreadCommand>,
    _evt_tx: mpsc::Sender<HashThreadEvent>,
    mut removal_rx: watch::Receiver<ThreadRemovalSignal>,
    status: Arc<RwLock<HashThreadStatus>>,
    mut chip_responses: R,
    mut chip_commands: W,
    mut peripherals: BoardPeripherals,
) where
    R: Stream<Item = Result<protocol::Response, std::io::Error>> + Unpin,
    W: Sink<protocol::Command> + Unpin,
    W::Error: std::fmt::Debug,
{
    // Disable ASIC on startup to establish known state
    if let Some(ref mut asic_enable) = peripherals.asic_enable {
        if let Err(e) = asic_enable.disable().await {
            warn!(error = %e, "Failed to disable ASIC on startup");
        }
    }

    let mut chip_initialized = false;
    let mut current_task: Option<HashTask> = None;
    let mut chip_jobs = ChipJobTracker::new();
    let mut ntime_ticker = tokio::time::interval(tokio::time::Duration::from_secs(1));
    ntime_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            // Removal signal (highest priority)
            _ = removal_rx.changed() => {
                let signal = removal_rx.borrow().clone();  // Clone to avoid holding borrow across await
                match signal {
                    ThreadRemovalSignal::Running => {
                        // False alarm - still running
                    }
                    _reason => {
                        // Update status
                        {
                            let mut s = status.write().unwrap();
                            s.is_active = false;
                        }

                        // Exit actor loop (channel closure signals removal to scheduler)
                        break;
                    }
                }
            }

            // Commands from scheduler
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    ThreadCommand::UpdateTask { new_task, response_tx } => {
                        if let Some(ref old) = current_task {
                            debug!(
                                old_job = %old.template.id,
                                new_job = %new_task.template.id,
                                "Updating work"
                            );
                        } else {
                            debug!(new_job = %new_task.template.id, "Updating work from idle");
                        }

                        if !chip_initialized {
                            trace!("Initializing chip on first assignment.");
                            if let Err(e) = initialize_chip(&mut chip_commands, &mut peripherals).await {
                                error!(error = %e, "Chip initialization failed");
                                response_tx.send(Err(e)).ok();
                                continue;
                            }
                            chip_initialized = true;
                        }

                        // Send initial job to chip
                        let chip_job_id = chip_jobs.insert(new_task.clone());
                        let old_task = current_task.replace(new_task.clone());
                        match task_to_job_full(&new_task, chip_job_id) {
                            Ok(job_data) => {
                                if let Err(e) = chip_commands.send(protocol::Command::JobFull { job_data }).await {
                                    error!(error = ?e, "Failed to send initial JobFull to chip");
                                    response_tx.send(Err(HashThreadError::WorkAssignmentFailed(
                                        format!("Failed to send job to chip: {:?}", e)
                                    ))).ok();
                                    continue;
                                } else {
                                    debug!("Sent initial job to chip");
                                }
                            }
                            Err(e) => {
                                error!(error = %e, "Failed to convert task to JobFull");
                                response_tx.send(Err(e)).ok();
                                continue;
                            }
                        }

                        {
                            let mut s = status.write().unwrap();
                            s.is_active = true;
                        }

                        response_tx.send(Ok(old_task)).ok();
                    }

                    ThreadCommand::ReplaceTask { new_task, response_tx } => {
                        if let Some(ref old) = current_task {
                            debug!(
                                old_job = %old.template.id,
                                new_job = %new_task.template.id,
                                "Replacing work"
                            );
                        } else {
                            debug!(new_job = %new_task.template.id, "Replacing work from idle");
                        }

                        if !chip_initialized {
                            trace!("Initializing chip on first assignment.");
                            if let Err(e) = initialize_chip(&mut chip_commands, &mut peripherals).await {
                                error!(error = %e, "Chip initialization failed");
                                response_tx.send(Err(e)).ok();
                                continue;
                            }
                            chip_initialized = true;
                        }

                        // Clear old jobs (old shares invalid)
                        chip_jobs.clear();

                        // Send initial job to chip
                        let chip_job_id = chip_jobs.insert(new_task.clone());
                        let old_task = current_task.replace(new_task.clone());
                        match task_to_job_full(&new_task, chip_job_id) {
                            Ok(job_data) => {
                                if let Err(e) = chip_commands.send(protocol::Command::JobFull { job_data }).await {
                                    error!(error = ?e, "Failed to send initial JobFull to chip");
                                    response_tx.send(Err(HashThreadError::WorkAssignmentFailed(
                                        format!("Failed to send job to chip: {:?}", e)
                                    ))).ok();
                                    continue;
                                } else {
                                    debug!("Sent initial job to chip (old work invalidated)");
                                }
                            }
                            Err(e) => {
                                error!(error = %e, "Failed to convert task to JobFull");
                                response_tx.send(Err(e)).ok();
                                continue;
                            }
                        }

                        {
                            let mut s = status.write().unwrap();
                            s.is_active = true;
                        }

                        response_tx.send(Ok(old_task)).ok();
                    }

                    ThreadCommand::GoIdle { response_tx } => {
                        debug!("Going idle");

                        let old_task = current_task.take();

                        {
                            let mut s = status.write().unwrap();
                            s.is_active = false;
                        }

                        response_tx.send(Ok(old_task)).ok();
                    }

                    ThreadCommand::Shutdown => {
                        info!("Shutdown command received");
                        // Exit actor loop (channel closure signals shutdown to scheduler)
                        break;
                    }
                }
            }

            // Chip responses from serial stream
            Some(result) = chip_responses.next() => {
                match result {
                    Ok(response) => {
                        match response {
                            protocol::Response::Nonce { nonce, job_id, version, midstate_num, subcore_id } => {
                                // Look up the task for this job_id
                                if let Some(task) = chip_jobs.get(job_id) {
                                    let template = task.template.as_ref();

                                    // Reconstruct full version from rolling field
                                    let full_version = version.apply_to_version(template.version.base());

                                    // Compute merkle root for this task's EN2
                                    match task.en2.as_ref().and_then(|en2| template.compute_merkle_root(en2).ok()) {
                                        Some(merkle_root) => {
                                            // Build block header
                                            let header = BlockHeader {
                                                version: full_version,
                                                prev_blockhash: template.prev_blockhash,
                                                merkle_root,
                                                time: task.ntime,
                                                bits: template.bits,
                                                nonce,
                                            };

                                            // Compute hash
                                            let hash = header.block_hash();

                                            // Validate against task share target
                                            if task.share_target.is_met_by(hash) {
                                                let expected_hashes = U256::from(task.share_target.to_work());
                                                let share = Share {
                                                    nonce,
                                                    hash,
                                                    version: full_version,
                                                    ntime: task.ntime,
                                                    extranonce2: task.en2,
                                                    expected_hashes,
                                                };

                                                // Send via task's dedicated channel
                                                if task.share_tx.send(share).await.is_err() {
                                                    // Channel closed = task replaced, share is stale
                                                    debug!("Share channel closed (task replaced)");
                                                } else {
                                                    debug!(
                                                        chip_job_id = job_id,
                                                        nonce = format!("{:#x}", nonce),
                                                        hash = %hash,
                                                        hash_diff = %Difficulty::from_hash(&hash),
                                                        target_diff = %Difficulty::from_target(task.share_target),
                                                        "Share found and sent"
                                                    );
                                                }
                                            } else {
                                                trace!(
                                                    chip_job_id = job_id,
                                                    nonce = format!("{:#x}", nonce),
                                                    hash = %hash,
                                                    hash_diff = %Difficulty::from_hash(&hash),
                                                    target_diff = %Difficulty::from_target(task.share_target),
                                                    "Nonce does not meet target (filtered)"
                                                );
                                            }
                                        }
                                        None => {
                                            error!(
                                                chip_job_id = job_id,
                                                "Failed to compute merkle root for nonce"
                                            );
                                        }
                                    }
                                } else {
                                    trace!(
                                        chip_job_id = job_id,
                                        nonce = format!("{:#x}", nonce),
                                        "Nonce for unknown job_id (possibly stale)"
                                    );
                                }

                                let _ = (midstate_num, subcore_id); // Unused for now
                            }

                            protocol::Response::ReadRegister { chip_address, register } => {
                                trace!(chip_address = %format!("0x{:02x}", chip_address), register = ?register, "Register read response");
                            }
                        }
                    }

                    Err(e) => {
                        error!(error = ?e, "Serial decode error");
                        // TODO: Emit error event, potentially trigger going offline if persistent
                    }
                }
            }

            // ntime rolling timer (roll forward every second)
            _ = ntime_ticker.tick(), if current_task.is_some() => {
                let task = current_task.as_mut().unwrap();

                // Increment ntime
                task.ntime += 1;

                // Convert to chip format and send
                match task_to_job_full(task, chip_jobs.insert(task.clone())) {
                    Ok(job_data) => {
                        if let Err(e) = chip_commands.send(protocol::Command::JobFull { job_data }).await {
                            error!(error = ?e, "Failed to send JobFull to chip");
                        } else {
                            trace!(ntime = task.ntime, "Sent ntime-rolled job to chip");
                        }
                    }
                    Err(e) => {
                        error!(error = %e, "Failed to convert task to JobFull");
                    }
                }
            }
        }
    }

    debug!("BM13xx thread actor exiting");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_task_to_job_full_converts_high_level_types() {
        use crate::asic::bm13xx::test_data::esp_miner_job;
        use crate::job_source::{
            Extranonce2, GeneralPurposeBits, JobTemplate, MerkleRootKind, VersionTemplate,
        };

        // Create a JobTemplate with test data values
        // Use MerkleRootKind::Fixed with the exact merkle_root from capture
        let template = Arc::new(JobTemplate {
            id: "test".into(),
            prev_blockhash: *esp_miner_job::wire_tx::PREV_BLOCKHASH,
            version: VersionTemplate::new(
                *esp_miner_job::wire_tx::VERSION,
                GeneralPurposeBits::full(),
            )
            .expect("Valid version template"),
            bits: *esp_miner_job::wire_tx::NBITS,
            share_target: crate::types::Difficulty::from(100_u64).to_target(),
            time: *esp_miner_job::wire_tx::NTIME,
            merkle_root: MerkleRootKind::Fixed(*esp_miner_job::wire_tx::MERKLE_ROOT),
        });

        // Dummy EN2 (doesn't matter since we're using Fixed merkle root)
        let dummy_en2 = Extranonce2::new(0, 1).unwrap();

        // Create dummy channel (not used in this test, just for struct construction)
        let (share_tx, _share_rx) = mpsc::channel(1);

        let task = HashTask {
            template,
            en2_range: None,
            en2: Some(dummy_en2),
            share_target: crate::types::Difficulty::from(100_u64).to_target(),
            ntime: *esp_miner_job::wire_tx::NTIME,
            share_tx,
        };

        // Convert to JobFullFormat
        let result = task_to_job_full(&task, *esp_miner_job::wire_tx::JOB_ID).unwrap();

        // Verify all fields match expected Bitcoin types
        assert_eq!(result.job_id, *esp_miner_job::wire_tx::JOB_ID);
        assert_eq!(result.num_midstates, 1);
        assert_eq!(result.starting_nonce, 0);
        assert_eq!(result.nbits, *esp_miner_job::wire_tx::NBITS);
        assert_eq!(result.ntime, *esp_miner_job::wire_tx::NTIME);
        assert_eq!(result.version, *esp_miner_job::wire_tx::VERSION);
        assert_eq!(
            result.prev_block_hash,
            *esp_miner_job::wire_tx::PREV_BLOCKHASH
        );
        assert_eq!(result.merkle_root, *esp_miner_job::wire_tx::MERKLE_ROOT);
    }
}
