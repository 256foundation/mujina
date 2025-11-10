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
use futures::{sink::Sink, stream::Stream};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_stream::StreamExt;

use super::{
    task::HashTask, HashThread, HashThreadCapabilities, HashThreadError, HashThreadEvent,
    HashThreadStatus,
};
use crate::{
    asic::bm13xx::{self, protocol},
    board::bitaxe::{BitaxePeripherals, ThreadRemovalSignal},
};

/// Command messages sent from scheduler to thread
#[derive(Debug)]
enum ThreadCommand {
    /// Update work (old shares still valid)
    UpdateWork {
        new_task: HashTask,
        response_tx: oneshot::Sender<std::result::Result<Option<HashTask>, HashThreadError>>,
    },

    /// Replace work (old shares invalid)
    ReplaceWork {
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
    /// Thread starts with chip in reset (uninit). Chip will be initialized when
    /// first work is assigned.
    ///
    /// # Arguments
    /// * `chip_responses` - Stream of decoded responses from chips
    /// * `chip_commands` - Sink for sending encoded commands to chips
    /// * `peripherals` - Shared peripheral handles (reset pin, voltage regulator)
    /// * `removal_rx` - Watch channel for board-triggered removal
    pub fn new<R, W>(
        chip_responses: R,
        chip_commands: W,
        peripherals: BitaxePeripherals,
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
            command_tx: cmd_tx,
            event_rx: Some(evt_rx),
            capabilities: HashThreadCapabilities {
                hashrate_estimate: 1_000_000_000.0, // Stub: 1 GH/s
            },
            status,
        }
    }
}

#[async_trait]
impl HashThread for BM13xxThread {
    fn capabilities(&self) -> &HashThreadCapabilities {
        &self.capabilities
    }

    async fn update_work(
        &mut self,
        new_work: HashTask,
    ) -> std::result::Result<Option<HashTask>, HashThreadError> {
        let (response_tx, response_rx) = oneshot::channel();

        self.command_tx
            .send(ThreadCommand::UpdateWork {
                new_task: new_work,
                response_tx,
            })
            .await
            .map_err(|_| HashThreadError::ChannelClosed("command channel closed".into()))?;

        response_rx
            .await
            .map_err(|_| HashThreadError::WorkAssignmentFailed("no response from thread".into()))?
    }

    async fn replace_work(
        &mut self,
        new_work: HashTask,
    ) -> std::result::Result<Option<HashTask>, HashThreadError> {
        let (response_tx, response_rx) = oneshot::channel();

        self.command_tx
            .send(ThreadCommand::ReplaceWork {
                new_task: new_work,
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
/// Releases chip from reset, configures all registers, and ramps frequency to
/// target. This is Bitaxe-specific initialization for a single BM1370 chip.
///
/// This function contains the complete chip initialization sequence that was
/// previously done by the board. The chip starts in reset and is configured
/// for mining when the scheduler assigns first work.
async fn initialize_chip<W>(
    chip_commands: &mut W,
    peripherals: &mut BitaxePeripherals,
) -> Result<(), HashThreadError>
where
    W: Sink<bm13xx::protocol::Command> + Unpin,
    W::Error: std::fmt::Debug,
{
    use crate::hw_trait::gpio::{GpioPin, PinValue};
    use futures::SinkExt;
    use protocol::{Command, Register};

    // Release from reset
    tracing::debug!("Releasing ASIC from reset");
    peripherals
        .asic_nrst
        .write(PinValue::High)
        .await
        .map_err(|e| {
            HashThreadError::InitializationFailed(format!("Failed to release reset: {}", e))
        })?;

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Send version mask configuration (3 times)
    tracing::debug!("Configuring version mask");
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
    tracing::debug!("Sending pre-configuration registers");

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
    tracing::debug!("Sending broadcast core configuration");

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
    use protocol::{Hashrate, ReportingInterval, ReportingRate, TicketMask};
    let reporting_interval = ReportingInterval::from_rate(
        Hashrate::gibihashes_per_sec(1024.0),
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
    tracing::debug!("Sending chip-specific configuration");

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

    // Frequency ramping (56.25 MHz → 525 MHz)
    tracing::debug!("Ramping frequency from 56.25 MHz to 525 MHz");
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
            tracing::trace!("Frequency ramp step {}/{}", i + 1, frequency_steps.len());
        }
    }

    tracing::debug!("Frequency ramping complete");

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

    tracing::info!("BM13xx chip initialized and ready for mining");

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
/// Thread starts with chip in reset (uninit). Chip is configured when scheduler
/// assigns first work.
async fn bm13xx_thread_actor<R, W>(
    mut cmd_rx: mpsc::Receiver<ThreadCommand>,
    evt_tx: mpsc::Sender<HashThreadEvent>,
    mut removal_rx: watch::Receiver<ThreadRemovalSignal>,
    status: Arc<RwLock<HashThreadStatus>>,
    mut chip_responses: R,
    mut chip_commands: W,
    mut peripherals: BitaxePeripherals,
) where
    R: Stream<Item = Result<bm13xx::protocol::Response, std::io::Error>> + Unpin,
    W: Sink<bm13xx::protocol::Command> + Unpin,
    W::Error: std::fmt::Debug,
{
    // Chip initialization state
    let mut chip_initialized = false;

    // Thread starts idle (no task)
    let mut current_task: Option<HashTask> = None;

    loop {
        tokio::select! {
            // Removal signal (highest priority)
            _ = removal_rx.changed() => {
                let signal = removal_rx.borrow().clone();  // Clone to avoid holding borrow across await
                match signal {
                    ThreadRemovalSignal::Running => {
                        // False alarm - still running
                    }
                    reason => {
                        tracing::info!("BM13xx thread removal: {:?}", reason);

                        // Send going offline event
                        evt_tx.send(HashThreadEvent::GoingOffline).await.ok();

                        // Update status
                        {
                            let mut s = status.write().unwrap();
                            s.is_active = false;
                        }

                        break;  // Exit actor loop on any removal reason
                    }
                }
            }

            // Commands from scheduler
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    ThreadCommand::UpdateWork { new_task, response_tx } => {
                        let job_desc = if let Some(ref old) = current_task {
                            format!("old={}, new={}", old.job_id, new_task.job_id)
                        } else {
                            format!("from idle, new={}", new_task.job_id)
                        };
                        tracing::debug!("BM13xx thread updating work: {}", job_desc);

                        // Initialize chip on first work assignment
                        if !chip_initialized {
                            tracing::info!("First work assignment - initializing chip");
                            if let Err(e) = initialize_chip(&mut chip_commands, &mut peripherals).await {
                                tracing::error!("Chip initialization failed: {}", e);
                                response_tx.send(Err(e)).ok();
                                continue;
                            }
                            chip_initialized = true;
                        }

                        // Return old task (None if was idle)
                        let old_task = current_task.replace(new_task);

                        // Update status
                        {
                            let mut s = status.write().unwrap();
                            s.is_active = true;
                        }

                        response_tx.send(Ok(old_task)).ok();

                        // TODO: Send new job to chips via serial
                    }

                    ThreadCommand::ReplaceWork { new_task, response_tx } => {
                        let job_desc = if let Some(ref old) = current_task {
                            format!("old={}, new={}", old.job_id, new_task.job_id)
                        } else {
                            format!("from idle, new={}", new_task.job_id)
                        };
                        tracing::debug!("BM13xx thread replacing work: {}", job_desc);

                        // Initialize chip on first work assignment
                        if !chip_initialized {
                            tracing::info!("First work assignment - initializing chip");
                            if let Err(e) = initialize_chip(&mut chip_commands, &mut peripherals).await {
                                tracing::error!("Chip initialization failed: {}", e);
                                response_tx.send(Err(e)).ok();
                                continue;
                            }
                            chip_initialized = true;
                        }

                        // Return old task (None if was idle)
                        let old_task = current_task.replace(new_task);

                        // Update status
                        {
                            let mut s = status.write().unwrap();
                            s.is_active = true;
                        }

                        response_tx.send(Ok(old_task)).ok();

                        // TODO: Send new job to chips via serial, invalidate old shares
                    }

                    ThreadCommand::GoIdle { response_tx } => {
                        tracing::debug!("BM13xx thread going idle");

                        // Take current task and go idle
                        let old_task = current_task.take();

                        // Update status
                        {
                            let mut s = status.write().unwrap();
                            s.is_active = false;  // Now idle
                        }

                        response_tx.send(Ok(old_task)).ok();

                        // TODO: Put chips in low power mode
                    }

                    ThreadCommand::Shutdown => {
                        tracing::info!("BM13xx thread shutting down");
                        evt_tx.send(HashThreadEvent::GoingOffline).await.ok();
                        break;
                    }
                }
            }

            // Chip responses from serial stream
            Some(result) = chip_responses.next() => {
                match result {
                    Ok(response) => {
                        match response {
                            bm13xx::protocol::Response::Nonce { nonce, job_id, version, midstate_num, subcore_id } => {
                                tracing::debug!(
                                    "Chip nonce: job_id={}, nonce=0x{:08x}, version=0x{:04x}, midstate={}, subcore={}",
                                    job_id, nonce, version, midstate_num, subcore_id
                                );
                                // TODO: Calculate hash, filter by pool_target, emit ShareFound event
                            }

                            bm13xx::protocol::Response::ReadRegister { chip_address, register } => {
                                tracing::trace!("Register read from chip {}: {:?}", chip_address, register);
                                // Ignore register reads for now
                            }
                        }
                    }

                    Err(e) => {
                        tracing::error!("Serial decode error: {:?}", e);
                        // TODO: Emit error event, potentially trigger going offline if persistent
                    }
                }
            }
        }
    }

    tracing::debug!("BM13xx thread actor exiting");
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;
    use std::{sync::Arc, time::Duration};
    use tokio::sync::Mutex;

    /// Create mock peripherals for testing
    ///
    /// Tests don't assign work (no initialization triggered), so this just
    /// needs to satisfy type requirements.
    fn mock_peripherals() -> BitaxePeripherals {
        use crate::hw_trait::gpio::Gpio;
        use crate::mgmt_protocol::{
            bitaxe_raw::{gpio::BitaxeRawGpioController, i2c::BitaxeRawI2c},
            ControlChannel,
        };
        use crate::peripheral::tps546::{Tps546, Tps546Config};

        let serial_stream = tokio_serial::SerialStream::pair().unwrap().0;
        let control_channel = ControlChannel::new(serial_stream);

        let mut gpio_controller = BitaxeRawGpioController::new(control_channel.clone());
        let i2c = BitaxeRawI2c::new(control_channel);

        let asic_nrst =
            futures::executor::block_on(async { gpio_controller.pin(0).await.unwrap() });

        let tps546 = Tps546::new(i2c, Tps546Config::bitaxe_gamma());

        BitaxePeripherals {
            asic_nrst,
            regulator: Arc::new(Mutex::new(tps546)),
        }
    }

    /// Create a mock response stream from a vector of responses
    ///
    /// Returns a stream that yields the given responses as Ok results.
    /// Useful for testing thread behavior with specific message sequences.
    fn mock_response_stream(
        responses: Vec<bm13xx::protocol::Response>,
    ) -> impl Stream<Item = Result<bm13xx::protocol::Response, std::io::Error>> {
        stream::iter(responses.into_iter().map(Ok))
    }

    /// Create a mock command sink that discards all commands
    ///
    /// Returns a sink that accepts commands but does nothing with them.
    /// Useful for tests that don't care about outgoing commands.
    fn mock_command_sink() -> futures::sink::Drain<bm13xx::protocol::Command> {
        futures::sink::drain()
    }

    #[tokio::test]
    async fn test_thread_creation() {
        let (_removal_tx, removal_rx) = watch::channel(ThreadRemovalSignal::Running);

        // Create empty streams for testing
        let responses = mock_response_stream(vec![]);
        let commands = mock_command_sink();

        let thread = BM13xxThread::new(responses, commands, mock_peripherals(), removal_rx);

        // Thread ID is based on task, not a debug name
        assert_eq!(thread.capabilities().hashrate_estimate, 1_000_000_000.0);
    }

    #[tokio::test]
    async fn test_removal_signal_triggers_going_offline() {
        let (removal_tx, removal_rx) = watch::channel(ThreadRemovalSignal::Running);
        let mut thread = BM13xxThread::new(
            mock_response_stream(vec![]),
            mock_command_sink(),
            mock_peripherals(),
            removal_rx,
        );

        // Take event receiver
        let mut event_rx = thread.take_event_receiver().unwrap();

        // Trigger removal with specific reason
        removal_tx
            .send(ThreadRemovalSignal::BoardDisconnected)
            .unwrap();

        // Should receive GoingOffline event
        let event = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .expect("timeout waiting for event")
            .expect("event channel closed");

        matches!(event, HashThreadEvent::GoingOffline);
    }

    #[tokio::test]
    async fn test_thread_processes_known_good_nonce() {
        use crate::job_source::test_blocks::block_881423;

        let (_removal_tx, removal_rx) = watch::channel(ThreadRemovalSignal::Running);

        // Create stream with known-good nonce from block 881423
        let responses = vec![bm13xx::protocol::Response::Nonce {
            nonce: block_881423::NONCE, // 0x5d6472f7 - actual winning nonce
            job_id: 1,
            // Extract version bits - chip returns top 16 bits it can roll
            version: (block_881423::VERSION.to_consensus() >> 16) as u16,
            midstate_num: 0,
            subcore_id: 0,
        }];

        let chip_responses = mock_response_stream(responses);
        let chip_commands = mock_command_sink();

        let thread = BM13xxThread::new(
            chip_responses,
            chip_commands,
            mock_peripherals(),
            removal_rx,
        );

        // Give actor time to process the nonce
        tokio::time::sleep(Duration::from_millis(50)).await;

        // For now, just verify thread doesn't crash when processing nonce
        // TODO: When ShareFound events are implemented, verify event is emitted
        let _status = thread.status();
        // Thread should still be running (not crashed)
    }
}
