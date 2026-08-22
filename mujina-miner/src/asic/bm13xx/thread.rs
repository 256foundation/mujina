//! BM13xx HashThread implementation.
//!
//! This module provides the HashThread implementation for BM13xx family ASIC
//! chips (BM1362, BM1366, BM1370, etc.). A BM13xxThread represents a chain of
//! BM13xx chips connected via a shared serial bus.
//!
//! The thread is implemented as an actor task that monitors the serial bus for
//! chip responses, filters shares, and manages work assignment.

use std::cmp::max;
use std::ops::RangeInclusive;
use std::sync::{Arc, RwLock};

use anyhow::{Context as _, Result, anyhow, bail};
use async_trait::async_trait;
use bitcoin::block::Header as BlockHeader;
use futures::{SinkExt, stream::Stream};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{self, Duration, MissedTickBehavior};

use super::chain::Chain;
use super::chip_config::ChipConfig;
use super::command::{
    ChainInactive, ChipCommandSink, Destination, JobCommand, JobFullFormat, RegisterCommand,
    SetChipAddress, SinkError, WriteRegister,
};
use super::reader::{Reader, ReaderChannels};
use super::register::{
    AdcCtrl1, CoreCommand, CoreRegister, IoDriverStrength, Log2Difficulty, MidstateConfig,
    PllDivider, Register, RegisterAddress, TicketMask,
};
use super::register_client::RegisterClient;
use super::response::{NonceResponse, RegisterResponse, Response};
use super::topology::TopologySpec;
use crate::{
    asic::hash_thread::{
        BoardPeripherals, HashTask, HashThread, HashThreadCapabilities, HashThreadEvent,
        HashThreadStatus, Share, ThreadRemovalSignal,
    },
    tracing::prelude::*,
    types::{Difficulty, Frequency, ShareRate},
};

/// BM13xx HashThread implementation.
///
/// Represents a chain of BM13xx chips as a schedulable worker. The thread
/// manages serial communication with chips, filters shares, and reports events.
/// Chain initialization happens lazily when first work is assigned.
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
    /// Thread starts with the chips held in reset. The chain will be
    /// initialized when first work is assigned.
    ///
    /// # Arguments
    /// * `name` - Human-readable name for logging (e.g., "Bitaxe Gamma (e2f56f9b)")
    /// * `config` - Chip model configuration (identity, PLL parameters)
    /// * `topology` - The board's declared chip wiring
    /// * `chip_responses` - Stream of decoded responses from chips
    /// * `chip_commands` - Sink for sending encoded commands to chips
    /// * `peripherals` - Hardware interfaces from board (reset line, regulator, etc.)
    /// * `removal_rx` - Watch channel for board-triggered removal
    pub fn new<R, W>(
        name: String,
        config: ChipConfig,
        topology: TopologySpec,
        chip_responses: R,
        chip_commands: W,
        peripherals: BoardPeripherals,
        removal_rx: watch::Receiver<ThreadRemovalSignal>,
    ) -> Self
    where
        R: Stream<Item = Result<Response, std::io::Error>> + Unpin + Send + 'static,
        W: ChipCommandSink + Unpin + Send + 'static,
        SinkError<W>: std::error::Error + Send + Sync + 'static,
    {
        let (command_tx, command_rx) = mpsc::channel(10);
        let (event_tx, event_rx) = mpsc::channel(100);

        let status = Arc::new(RwLock::new(HashThreadStatus::default()));

        let (reader, channels) = Reader::spawn(chip_responses);
        let actor = Actor::new(
            config,
            topology,
            event_tx,
            Arc::clone(&status),
            chip_commands,
            peripherals,
            reader,
        );
        tokio::spawn(actor.run(command_rx, removal_rx, channels));

        Self {
            name,
            command_tx,
            event_rx: Some(event_rx),
            capabilities: HashThreadCapabilities::default(),
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

    async fn configure(&mut self) -> Result<()> {
        self.command_tx
            .send(ThreadCommand::Configure)
            .await
            .map_err(|_| anyhow!("command channel closed"))
    }

    async fn update_task(&mut self, new_task: HashTask) -> Result<Option<HashTask>> {
        let (response_tx, response_rx) = oneshot::channel();

        self.command_tx
            .send(ThreadCommand::UpdateTask {
                new_task,
                response_tx,
            })
            .await
            .map_err(|_| anyhow!("command channel closed"))?;

        response_rx
            .await
            .map_err(|_| anyhow!("no response from thread"))?
    }

    async fn replace_task(&mut self, new_task: HashTask) -> Result<Option<HashTask>> {
        let (response_tx, response_rx) = oneshot::channel();

        self.command_tx
            .send(ThreadCommand::ReplaceTask {
                new_task,
                response_tx,
            })
            .await
            .map_err(|_| anyhow!("command channel closed"))?;

        response_rx
            .await
            .map_err(|_| anyhow!("no response from thread"))?
    }

    async fn go_idle(&mut self) -> Result<Option<HashTask>> {
        let (response_tx, response_rx) = oneshot::channel();

        self.command_tx
            .send(ThreadCommand::GoIdle { response_tx })
            .await
            .map_err(|_| anyhow!("command channel closed"))?;

        response_rx
            .await
            .map_err(|_| anyhow!("no response from thread"))?
    }

    fn take_event_receiver(&mut self) -> Option<mpsc::Receiver<HashThreadEvent>> {
        self.event_rx.take()
    }

    fn status(&self) -> HashThreadStatus {
        self.status.read().unwrap().clone()
    }
}

/// Command messages sent from scheduler to thread
#[derive(Debug)]
enum ThreadCommand {
    /// Declare expected hashrate and ready the thread for work
    Configure,

    /// Update task (old shares still valid)
    UpdateTask {
        new_task: HashTask,
        response_tx: oneshot::Sender<Result<Option<HashTask>>>,
    },

    /// Replace task (old shares invalid)
    ReplaceTask {
        new_task: HashTask,
        response_tx: oneshot::Sender<Result<Option<HashTask>>>,
    },

    /// Go idle (stop hashing, low power)
    GoIdle {
        response_tx: oneshot::Sender<Result<Option<HashTask>>>,
    },

    /// Shutdown the thread
    #[expect(unused)]
    Shutdown,
}

/// Internal actor for BM13xxThread.
///
/// The channels the select loop awaits are `run` parameters rather
/// than fields, so the loop can borrow them independently of the
/// actor state.
struct Actor<W> {
    /// Chip model configuration (identity, PLL parameters).
    config: ChipConfig,

    /// Live model of the chip chain, built from the board's declared
    /// topology.
    chain: Chain,

    /// Event channel to the scheduler.
    event_tx: mpsc::Sender<HashThreadEvent>,

    /// Shared status, read by the handle.
    status: Arc<RwLock<HashThreadStatus>>,

    /// Sink for sending encoded commands to chips.
    chip_commands: W,

    /// Hardware interfaces from the board (reset line, regulator, etc.).
    peripherals: BoardPeripherals,

    /// Owner of the response demux task. Held only so the task is
    /// aborted, releasing the serial stream, when the actor exits.
    _reader: Reader,

    /// ASIC ticket mask difficulty.
    asic_difficulty: Log2Difficulty,

    /// Whether lazy chain initialization has run.
    chain_initialized: bool,

    /// The task currently being hashed.
    current_task: Option<HashTask>,

    /// Tasks sent to the chip, by chip job id.
    chip_jobs: ChipJobTracker,
}

impl<W> Actor<W>
where
    W: ChipCommandSink + Unpin,
    SinkError<W>: std::error::Error + Send + Sync + 'static,
{
    fn new(
        config: ChipConfig,
        topology: TopologySpec,
        event_tx: mpsc::Sender<HashThreadEvent>,
        status: Arc<RwLock<HashThreadStatus>>,
        chip_commands: W,
        peripherals: BoardPeripherals,
        reader: Reader,
    ) -> Self {
        // ASIC ticket mask difficulty: ~1 nonce/sec at nameplate rate
        let asic_difficulty = Log2Difficulty::from_difficulty(
            ShareRate::per_second(1.0).to_difficulty(config.nameplate),
        );

        Self {
            config,
            chain: Chain::from_topology(&topology),
            event_tx,
            status,
            chip_commands,
            peripherals,
            _reader: reader,
            asic_difficulty,
            chain_initialized: false,
            current_task: None,
            chip_jobs: ChipJobTracker::new(),
        }
    }

    /// Runs the actor loop until removal, shutdown, or channel closure.
    ///
    /// Handles commands from the scheduler (update/replace work, go
    /// idle, shutdown), the removal signal from the board (USB unplug,
    /// fault, etc.), and the demuxed chip responses from the reader.
    /// Reset is asserted on startup to establish known state; the
    /// chain is initialized lazily when the scheduler assigns first
    /// work.
    async fn run(
        mut self,
        mut command_rx: mpsc::Receiver<ThreadCommand>,
        mut removal_rx: watch::Receiver<ThreadRemovalSignal>,
        channels: ReaderChannels,
    ) {
        let ReaderChannels {
            mut nonces,
            mut register_responses,
        } = channels;

        // Assert reset on startup to establish known state
        if let Err(e) = self.peripherals.reset_line.assert().await {
            warn!(error = %e, "Failed to assert chip reset on startup");
        }

        let mut ntime_ticker = time::interval(Duration::from_secs(1));
        ntime_ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

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
                            self.set_active(false);

                            // Exit actor loop (channel closure signals removal to scheduler)
                            break;
                        }
                    }
                }

                // Commands from scheduler
                Some(cmd) = command_rx.recv() => {
                    match cmd {
                        ThreadCommand::Configure => self.configure().await,

                        ThreadCommand::UpdateTask { new_task, response_tx } => {
                            self.assign_task(new_task, response_tx, false, &mut register_responses).await;
                        }

                        ThreadCommand::ReplaceTask { new_task, response_tx } => {
                            self.assign_task(new_task, response_tx, true, &mut register_responses).await;
                        }

                        ThreadCommand::GoIdle { response_tx } => {
                            debug!("Going idle");

                            let old_task = self.current_task.take();
                            self.set_active(false);
                            response_tx.send(Ok(old_task)).ok();
                        }

                        ThreadCommand::Shutdown => {
                            info!("Shutdown command received");
                            // Exit actor loop (channel closure signals shutdown to scheduler)
                            break;
                        }
                    }
                }

                // Nonce reports from the chips
                Some(nonce) = nonces.recv() => {
                    self.handle_nonce(nonce).await;
                }

                // Replies to register conversations; nothing asks
                // yet, so log and discard
                Some(response) = register_responses.recv() => {
                    trace!(
                        chip_address = %format!("0x{:02x}", response.chip_address),
                        register = ?response.register,
                        "Register read response"
                    );
                }

                // ntime rolling timer (roll forward every second)
                _ = ntime_ticker.tick(), if self.current_task.is_some() => {
                    self.roll_ntime().await;
                }
            }
        }

        debug!("BM13xx thread actor exiting");
    }

    /// Declares the thread's expected hashrate to the scheduler.
    async fn configure(&mut self) {
        // Nameplate rate for one chip; a rough stand-in for a real
        // frequency-derived estimate.
        let expected = self.config.nameplate;
        if self
            .event_tx
            .send(HashThreadEvent::ExpectedHashRate(expected))
            .await
            .is_err()
        {
            debug!("Event channel closed during configure");
        }
    }

    /// Takes a new task and sends its first job to the chip,
    /// initializing the chain on the first assignment. `replace`
    /// forgets prior jobs, invalidating their shares.
    async fn assign_task(
        &mut self,
        new_task: HashTask,
        response_tx: oneshot::Sender<Result<Option<HashTask>>>,
        replace: bool,
        register_responses: &mut mpsc::Receiver<RegisterResponse>,
    ) {
        let verb = if replace { "Replacing" } else { "Updating" };
        if let Some(ref old) = self.current_task {
            debug!(
                old_job = %old.template.id,
                new_job = %new_task.template.id,
                "{verb} work"
            );
        } else {
            debug!(new_job = %new_task.template.id, "{verb} work from idle");
        }

        if !self.chain_initialized {
            trace!("Initializing chain on first assignment.");
            if let Err(e) = self.initialize_chain(register_responses).await {
                error!(error = %e, "Chain initialization failed");
                response_tx.send(Err(e)).ok();
                return;
            }
            self.chain_initialized = true;
        }

        if replace {
            // Clear old jobs (old shares invalid)
            self.chip_jobs.clear();
        }

        // Send initial job to chip
        let chip_job_id = self.chip_jobs.insert(new_task.clone());
        let old_task = self.current_task.replace(new_task.clone());
        match task_to_job_full(&new_task, chip_job_id) {
            Ok(job_data) => {
                if let Err(e) = self.chip_commands.send(JobCommand::JobFull(job_data)).await {
                    error!(error = ?e, "Failed to send initial JobFull to chip");
                    let err = anyhow!("failed to send job to chip: {e:?}");
                    response_tx.send(Err(err)).ok();
                    return;
                } else if replace {
                    debug!("Sent initial job to chip (old work invalidated)");
                } else {
                    debug!("Sent initial job to chip");
                }
            }
            Err(e) => {
                error!(error = %e, "Failed to convert task to JobFull");
                response_tx.send(Err(e)).ok();
                return;
            }
        }

        self.set_active(true);
        response_tx.send(Ok(old_task)).ok();
    }

    /// Initializes the chip chain for mining.
    ///
    /// Powers the core rail, releases the chips from reset,
    /// enumerates them against the declared topology, assigns
    /// addresses, configures all registers, and ramps the frequency
    /// to target.
    async fn initialize_chain(
        &mut self,
        register_responses: &mut mpsc::Receiver<RegisterResponse>,
    ) -> Result<()> {
        // Power the core rail before releasing reset
        debug!("Enabling core voltage");
        self.peripherals
            .voltage_regulator
            .enable()
            .await
            .context("failed to enable core voltage")?;
        time::sleep(Duration::from_millis(500)).await;

        // Release the chips from reset
        debug!("Releasing chip reset");
        self.peripherals
            .reset_line
            .release()
            .await
            .context("failed to release chip reset")?;

        time::sleep(Duration::from_millis(200)).await;

        // Send version mask configuration (3 times)
        debug!("Configuring version mask");
        for _ in 1..=3 {
            self.chip_commands
                .send(RegisterCommand::WriteRegister(WriteRegister {
                    destination: Destination::Broadcast,
                    register: Register::MidstateConfig(MidstateConfig::full_rolling()),
                }))
                .await
                .context("failed to send version mask")?;
            time::sleep(Duration::from_millis(5)).await;
        }

        time::sleep(Duration::from_millis(10)).await;

        // Enumerate the chips and check them against the declared
        // topology. The version mask above switched the chips to the
        // 11-byte response format the codec parses.
        debug!("Enumerating chips");
        let replies = RegisterClient::new(&mut self.chip_commands, register_responses)
            .broadcast_read(RegisterAddress::ChipId)
            .await
            .context("chip enumeration failed")?;
        if replies.len() != self.chain.chip_count() {
            bail!(
                "found {} chips, declared topology has {}",
                replies.len(),
                self.chain.chip_count()
            );
        }
        debug!(chips = replies.len(), "Chip enumeration complete");

        // Pre-configuration registers
        debug!("Sending pre-configuration registers");

        self.chip_commands
            .send(RegisterCommand::WriteRegister(WriteRegister {
                destination: Destination::Broadcast,
                register: Register::SoftResetControl(self.config.soft_reset_defaults),
            }))
            .await?;
        self.chip_commands
            .send(RegisterCommand::WriteRegister(WriteRegister {
                destination: Destination::Broadcast,
                register: Register::MiscControl(self.config.misc_control),
            }))
            .await?;

        self.chip_commands
            .send(RegisterCommand::ChainInactive(ChainInactive))
            .await
            .context("failed to send ChainInactive")?;

        // Address the chips in chain order. After ChainInactive, the
        // first unaddressed chip adopts each SetChipAddress and
        // forwards later ones downstream, so one command per chip
        // addresses the whole chain.
        self.chain
            .assign_addresses()
            .context("chip address assignment failed")?;
        for (_, chip) in self.chain.chips() {
            self.chip_commands
                .send(RegisterCommand::SetChipAddress(SetChipAddress {
                    chip_address: chip.address,
                }))
                .await
                .context("failed to send SetChipAddress")?;
        }

        // Core configuration (broadcast)
        debug!("Sending broadcast core configuration");

        self.chip_commands
            .send(RegisterCommand::WriteRegister(WriteRegister {
                destination: Destination::Broadcast,
                register: Register::CoreMailbox(self.config.clock_select),
            }))
            .await?;
        self.chip_commands
            .send(RegisterCommand::WriteRegister(WriteRegister {
                destination: Destination::Broadcast,
                register: Register::CoreMailbox(CoreCommand::write_all(
                    CoreRegister::ClockDelay,
                    0x0C,
                )),
            }))
            .await?;

        // Ticket mask
        let ticket_mask = TicketMask::new(self.asic_difficulty);

        self.chip_commands
            .send(RegisterCommand::WriteRegister(WriteRegister {
                destination: Destination::Broadcast,
                register: Register::TicketMask(ticket_mask),
            }))
            .await?;
        self.chip_commands
            .send(RegisterCommand::WriteRegister(WriteRegister {
                destination: Destination::Broadcast,
                register: Register::IoDriverStrength(IoDriverStrength::normal()),
            }))
            .await?;

        // Chip-specific configuration
        debug!("Sending chip-specific configuration");

        for (_, chip) in self.chain.chips() {
            let destination = Destination::Chip(chip.address);
            self.chip_commands
                .send(RegisterCommand::WriteRegister(WriteRegister {
                    destination,
                    register: Register::SoftResetControl(self.config.core_reset),
                }))
                .await?;
            self.chip_commands
                .send(RegisterCommand::WriteRegister(WriteRegister {
                    destination,
                    register: Register::MiscControl(self.config.misc_control),
                }))
                .await?;
            self.chip_commands
                .send(RegisterCommand::WriteRegister(WriteRegister {
                    destination,
                    register: Register::CoreMailbox(self.config.clock_select),
                }))
                .await?;
            self.chip_commands
                .send(RegisterCommand::WriteRegister(WriteRegister {
                    destination,
                    register: Register::CoreMailbox(CoreCommand::write_all(
                        CoreRegister::ClockDelay,
                        0x0C,
                    )),
                }))
                .await?;
            self.chip_commands
                .send(RegisterCommand::WriteRegister(WriteRegister {
                    destination,
                    register: Register::CoreMailbox(CoreCommand::write_all(
                        CoreRegister::CoreEnable,
                        0xAA,
                    )),
                }))
                .await?;
        }

        // Additional settings
        self.chip_commands
            .send(RegisterCommand::WriteRegister(WriteRegister {
                destination: Destination::Broadcast,
                register: Register::AdcCtrl1(AdcCtrl1::bring_up()),
            }))
            .await?;
        self.chip_commands
            .send(RegisterCommand::WriteRegister(WriteRegister {
                destination: Destination::Broadcast,
                register: Register::AnalogMux(self.config.analog_mux),
            }))
            .await?;
        self.chip_commands
            .send(RegisterCommand::WriteRegister(WriteRegister {
                destination: Destination::Broadcast,
                register: Register::AdcCtrl1(AdcCtrl1::bring_up()),
            }))
            .await?;
        self.chip_commands
            .send(RegisterCommand::WriteRegister(WriteRegister {
                destination: Destination::Broadcast,
                register: Register::CoreMailbox(CoreCommand::nonce_bin_overflow(true)),
            }))
            .await?;

        // Frequency ramping from the reset frequency to the
        // configured target
        let ramp = *self.config.freq_range.start()..=self.config.default_freq;
        debug!(
            "Ramping frequency from {} MHz to {} MHz",
            ramp.start().mhz(),
            ramp.end().mhz()
        );
        let frequency_steps =
            generate_frequency_ramp_steps(&self.config, ramp, self.config.ramp_step);

        for (i, pll_config) in frequency_steps.iter().enumerate() {
            self.chip_commands
                .send(RegisterCommand::WriteRegister(WriteRegister {
                    destination: Destination::Broadcast,
                    register: Register::PllDivider(*pll_config),
                }))
                .await
                .context("PLL ramp failed")?;

            time::sleep(Duration::from_millis(100)).await;

            if i % 10 == 0 || i == frequency_steps.len() - 1 {
                trace!("Frequency ramp step {}/{}", i + 1, frequency_steps.len());
            }
        }

        debug!("Frequency ramping complete");

        // Final configuration
        self.chip_commands
            .send(RegisterCommand::WriteRegister(WriteRegister {
                destination: Destination::Broadcast,
                register: Register::HashCountingNumber(self.config.hash_counting_number),
            }))
            .await?;
        self.chip_commands
            .send(RegisterCommand::WriteRegister(WriteRegister {
                destination: Destination::Broadcast,
                register: Register::MidstateConfig(MidstateConfig::full_rolling()),
            }))
            .await?;

        time::sleep(Duration::from_millis(150)).await;

        // Verify bring-up by reading configuration back from every
        // chip at its assigned address; a directed read answered at
        // that address also proves the chip took it.
        debug!("Verifying chain configuration");
        let target_pll = self
            .config
            .calculate_pll(self.config.default_freq)
            .context("no PLL solution for the target frequency")?;
        // What each chip should answer. The registers answer as
        // written, except PLL_DIVIDER bit 31, the lock report
        // (LOCKED), which a healthy chip answers set after the ramp.
        let expected = [
            Register::MiscControl(self.config.misc_control),
            Register::TicketMask(ticket_mask),
            Register::PllDivider(PllDivider {
                locked: true,
                ..target_pll
            }),
        ];
        let addresses: Vec<u8> = self.chain.chips().map(|(_, chip)| chip.address).collect();
        let mut client = RegisterClient::new(&mut self.chip_commands, register_responses);
        for address in addresses {
            for register in &expected {
                let actual = client
                    .read(address, register.address())
                    .await
                    .context("bring-up verification read failed")?;
                if actual != *register {
                    bail!(
                        "chip 0x{address:02x} readback mismatch: \
                         expected {register:?}, read {actual:?}"
                    );
                }
            }
        }
        debug!("Chain configuration verified");

        Ok(())
    }

    /// Handles one nonce report from the chips.
    async fn handle_nonce(&mut self, nonce_response: NonceResponse) {
        let NonceResponse {
            nonce,
            job_id,
            version,
            excess_difficulty,
            subcore_id,
        } = nonce_response;

        // Look up the task for this job_id
        if let Some(task) = self.chip_jobs.get(job_id) {
            let template = task.template.as_ref();

            // Reconstruct full version from rolling field
            let full_version = version.apply_to_version(template.version.base());

            // Compute merkle root for this task's EN2
            match task
                .en2
                .as_ref()
                .and_then(|en2| template.compute_merkle_root(en2).ok())
            {
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
                        // Attribute work at the harder of the
                        // ASIC ticket mask and the scheduler
                        // target, since the actual filter is
                        // whichever is stricter.
                        let expected_work =
                            max(self.asic_difficulty.to_work(), task.share_target.to_work());

                        let share = Share {
                            nonce,
                            hash,
                            version: full_version,
                            ntime: task.ntime,
                            extranonce2: task.en2,
                            expected_work,
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

        let _ = (excess_difficulty, subcore_id); // Unused for now
    }

    /// Rolls the current task's ntime forward and sends the job to
    /// the chip.
    async fn roll_ntime(&mut self) {
        let task = self.current_task.as_mut().unwrap();

        // Increment ntime
        task.ntime += 1;

        // Convert to chip format and send
        match task_to_job_full(task, self.chip_jobs.insert(task.clone())) {
            Ok(job_data) => {
                if let Err(e) = self.chip_commands.send(JobCommand::JobFull(job_data)).await {
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

    /// Updates the shared active flag.
    fn set_active(&self, is_active: bool) {
        self.status.write().unwrap().is_active = is_active;
    }
}

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

/// Convert HashTask to JobFullFormat for chip hardware.
///
/// Extracts or computes the merkle root, then builds a JobFullFormat with all
/// block header fields. For computed merkle roots, requires EN2. For fixed merkle
/// roots (Stratum v2 header-only), uses the template's fixed value directly.
fn task_to_job_full(task: &HashTask, chip_job_id: u8) -> Result<JobFullFormat> {
    use crate::job_source::MerkleRootKind;

    let template = task.template.as_ref();

    // Get merkle root (computed or fixed)
    let merkle_root = match &template.merkle_root {
        MerkleRootKind::Computed(_) => {
            // Extract EN2 (required for computed merkle roots)
            let en2 = task
                .en2
                .as_ref()
                .ok_or_else(|| anyhow!("EN2 required for computed merkle root"))?;

            // Compute merkle root for this EN2
            template
                .compute_merkle_root(en2)
                .context("merkle root computation failed")?
        }
        MerkleRootKind::Fixed(merkle_root) => *merkle_root,
    };

    Ok(JobFullFormat {
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

/// Generate frequency ramp steps for smooth PLL transitions
fn generate_frequency_ramp_steps(
    config: &ChipConfig,
    range: RangeInclusive<Frequency>,
    step: Frequency,
) -> Vec<PllDivider> {
    let target = *range.end();
    let mut configs = Vec::new();
    let mut current = *range.start();

    while current <= target {
        if let Some(pll) = config.calculate_pll(current) {
            configs.push(pll);
        }
        let next = Frequency::from_hz(current.hz() + step.hz());
        // A final short step ends the ramp exactly on the target
        current = if next > target && current < target {
            target
        } else {
            next
        };
    }

    configs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asic::bm13xx::chip_config;

    #[test]
    fn ramp_covers_range_in_steps() {
        let config = chip_config::bm1370();
        let steps = generate_frequency_ramp_steps(
            &config,
            Frequency::from_mhz(56.25)..=Frequency::from_mhz(525.0),
            Frequency::from_mhz(6.25),
        );

        // 75 steps of 6.25 MHz above the starting frequency
        assert_eq!(steps.len(), 76);

        // Each step is the solver's answer for the next stepped
        // frequency
        for (i, step) in steps.iter().enumerate() {
            let freq = Frequency::from_hz(56_250_000 + i as u64 * 6_250_000);
            assert_eq!(*step, config.calculate_pll(freq).unwrap(), "step {i}");
        }
    }

    #[test]
    fn ramp_ends_on_target_when_step_overshoots() {
        let config = chip_config::bm1370();
        let steps = generate_frequency_ramp_steps(
            &config,
            Frequency::from_mhz(56.25)..=Frequency::from_mhz(60.0),
            Frequency::from_mhz(6.25),
        );

        let expected = [
            config.calculate_pll(Frequency::from_mhz(56.25)).unwrap(),
            config.calculate_pll(Frequency::from_mhz(60.0)).unwrap(),
        ];
        assert_eq!(steps, expected);
    }

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
