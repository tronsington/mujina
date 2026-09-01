//! BM13xx HashThread implementation.
//!
//! This module provides the HashThread implementation for BM13xx family ASIC
//! chips (BM1362, BM1366, BM1370, etc.). A BM13xxThread represents a chain of
//! BM13xx chips connected via a shared serial bus.
//!
//! The thread is implemented as an actor task that monitors the serial bus for
//! chip responses, filters shares, and manages work assignment.

use std::cmp::max;
use std::ops::{ControlFlow, RangeInclusive};
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
use super::peripherals::BoardPeripherals;
use super::reader::{Reader, ReaderChannels};
use super::register::{
    AdcCtrl1, AnalogMux, CoreCommand, CoreRegister, HashCountingNumber, IoDriverStrength,
    Log2Difficulty, MidstateConfig, MiscControl, PllDivider, Register, RegisterAddress,
    SoftResetControl, TicketMask, UartBaud,
};
use super::register_client::RegisterClient;
use super::response::{NonceResponse, RegisterResponse, Response};
use super::topology::TopologySpec;
use crate::{
    asic::hash_thread::{
        HashTask, HashThread, HashThreadCapabilities, HashThreadEvent, HashThreadStatus, Share,
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
    /// * `shutdown_rx` - Shutdown signal from the board; a send or a
    ///   dropped sender requests shutdown
    /// * `bring_up_override` - Skip the shared, config-driven bring-up
    ///   in favor of a board-supplied sequence. `None` for every board
    ///   whose real captured wire behavior fits `ChipConfig`/`TopologySpec`
    ///   (Bitaxe Gamma today). See [`Bm1366ChainBringUp`] for the one
    ///   board that doesn't yet.
    #[expect(clippy::too_many_arguments)]
    pub fn new<R, W>(
        name: String,
        config: ChipConfig,
        topology: TopologySpec,
        chip_responses: R,
        chip_commands: W,
        peripherals: BoardPeripherals,
        shutdown_rx: watch::Receiver<()>,
        bring_up_override: Option<Bm1366ChainBringUp>,
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
            bring_up_override,
        );
        tokio::spawn(actor.run(command_rx, shutdown_rx, channels));

        Self {
            name,
            command_tx,
            event_rx: Some(event_rx),
            capabilities: HashThreadCapabilities::default(),
            status,
        }
    }
}

/// A single per-chip addressed register write: (chip_address,
/// register_address, raw 4-byte wire data). Used by
/// [`Bm1366ChainBringUp`] to replay a captured domain config table
/// verbatim -- see `board/antminer_s19k_am3.rs` for where these
/// values come from.
pub type DomainConfigWrite = (u8, RegisterAddress, [u8; 4]);

/// Reconfigures a chip UART's baud rate on the host side, after the
/// chip side is told to switch via a `UartBaud` register write.
///
/// Deliberately transport-agnostic (this module has no dependency on
/// `crate::transport`) -- boards that need a real baud switch during
/// bring-up (see [`Bm1366ChainBringUp`]) implement this against
/// whatever concrete serial transport they use.
pub trait BaudControl: Send + Sync {
    fn set_baud_rate(&self, baud: u32) -> Result<()>;
}

/// Board-specific override of the shared, config-driven bring-up in
/// `Actor::initialize_chain`, for a chain whose real captured wire
/// behavior doesn't fit `ChipConfig`/`TopologySpec` yet.
///
/// Currently exists for exactly one board: the Antminer S19K Pro's
/// BM1366 chain. See `initialize_chip_bm1366_chain`'s doc comment for
/// why this chain's real captured sequence (register values,
/// ordering, a mid-bring-up baud switch) diverges from the shared
/// path rather than being expressed as new `ChipConfig` fields --
/// several of those values (e.g. ClockDelay, the per-chip MiscControl
/// value) genuinely differ from what the shared path currently sends
/// for every other chip model, and this chain's bring-up is still an
/// open investigation (see the doc comment below), not settled
/// behavior to generalize from yet.
pub struct Bm1366ChainBringUp {
    pub chip_count: u8,
    pub domain_config: &'static [DomainConfigWrite],
    /// If present, switches both the chip side (`UartBaud` register
    /// write) and the host tty to `bosminer`'s real confirmed
    /// 3,125,000 operating baud after the rest of bring-up completes.
    /// `None` stays at the discovery-time 115200 baud throughout.
    pub baud_control: Option<Box<dyn BaudControl>>,
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

    /// Board-supplied override of the shared bring-up in
    /// `initialize_chain`, for chains whose real captured wire
    /// behavior doesn't fit `ChipConfig`/`TopologySpec` yet. See
    /// [`Bm1366ChainBringUp`].
    bring_up_override: Option<Bm1366ChainBringUp>,

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
    #[expect(clippy::too_many_arguments)]
    fn new(
        config: ChipConfig,
        topology: TopologySpec,
        event_tx: mpsc::Sender<HashThreadEvent>,
        status: Arc<RwLock<HashThreadStatus>>,
        chip_commands: W,
        peripherals: BoardPeripherals,
        reader: Reader,
        bring_up_override: Option<Bm1366ChainBringUp>,
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
            bring_up_override,
            current_task: None,
            chip_jobs: ChipJobTracker::new(),
        }
    }

    /// Runs the actor loop until shutdown or channel closure.
    ///
    /// Handles commands from the scheduler (update/replace work, go
    /// idle), the shutdown signal from the board (USB unplug,
    /// fault, etc.), and the demuxed chip responses from the reader.
    /// Reset is asserted on startup to establish known state; the
    /// chain is initialized lazily when the scheduler assigns first
    /// work.
    async fn run(
        mut self,
        mut command_rx: mpsc::Receiver<ThreadCommand>,
        mut shutdown_rx: watch::Receiver<()>,
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
                // Shutdown signal (highest priority); an error means
                // the board dropped the sender, which requests
                // shutdown too
                _ = shutdown_rx.changed() => {
                    self.set_active(false);

                    // Exit actor loop (channel closure signals exit to the scheduler)
                    break;
                }

                // Commands from scheduler
                cmd = command_rx.recv() => {
                    let Some(cmd) = cmd else {
                        debug!("Thread handle dropped");
                        break;
                    };
                    match cmd {
                        ThreadCommand::Configure => self.configure().await,

                        ThreadCommand::UpdateTask { new_task, response_tx } => {
                            let flow = self
                                .assign_task(new_task, response_tx, false, &mut shutdown_rx, &mut register_responses)
                                .await;
                            if flow.is_break() {
                                break;
                            }
                        }

                        ThreadCommand::ReplaceTask { new_task, response_tx } => {
                            let flow = self
                                .assign_task(new_task, response_tx, true, &mut shutdown_rx, &mut register_responses)
                                .await;
                            if flow.is_break() {
                                break;
                            }
                        }

                        ThreadCommand::GoIdle { response_tx } => {
                            debug!("Going idle");

                            let old_task = self.current_task.take();
                            self.set_active(false);
                            response_tx.send(Ok(old_task)).ok();
                        }
                    }
                }

                // Nonce reports from the chips
                nonce = nonces.recv() => {
                    let Some(nonce) = nonce else {
                        warn!("Chip response stream ended");
                        break;
                    };
                    self.handle_nonce(nonce).await;
                }

                // Replies to register conversations; nothing asks
                // yet, so log and discard
                response = register_responses.recv() => {
                    let Some(response) = response else {
                        warn!("Chip response stream ended");
                        break;
                    };
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

        self.disable_chain().await;
        debug!("BM13xx thread actor exiting");
    }

    /// Asserts the chips' reset, then disables the core rail.
    /// Idempotent, and safe before bring-up. On an unplugged
    /// board both writes fail; the warnings are all that can be
    /// done.
    async fn disable_chain(&mut self) {
        if let Err(e) = self.peripherals.reset_line.assert().await {
            warn!(error = %e, "Failed to assert chip reset on exit");
        }
        if let Err(e) = self.peripherals.voltage_regulator.disable().await {
            warn!(error = %e, "Failed to disable core voltage on exit");
        }
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
    /// forgets prior jobs, invalidating their shares. Returns
    /// `Break` when the actor must exit because the board's
    /// shutdown signal cut bring-up short.
    async fn assign_task(
        &mut self,
        new_task: HashTask,
        response_tx: oneshot::Sender<Result<Option<HashTask>>>,
        replace: bool,
        shutdown_rx: &mut watch::Receiver<()>,
        register_responses: &mut mpsc::Receiver<RegisterResponse>,
    ) -> ControlFlow<()> {
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

        // The select watches for shutdown while bring-up runs, so
        // every await point in bring-up is an abort point.
        // Dropping the half-done future is safe because the actor
        // disables the chain on exit whatever the bring-up
        // progress.
        tokio::select! {
            result = self.ensure_chain_initialized(register_responses) => {
                if let Err(e) = result {
                    error!(error = %e, "Chain initialization failed");
                    response_tx.send(Err(e)).ok();
                    return ControlFlow::Continue(());
                }
            }

            _ = shutdown_rx.changed() => {
                debug!("Shutdown requested during bring-up");
                response_tx.send(Err(anyhow!("shut down during bring-up"))).ok();
                return ControlFlow::Break(());
            }
        }

        if replace {
            // Clear old jobs (old shares invalid)
            self.chip_jobs.clear();
        }

        // Send the task's first job; the ntime roller sends the rest
        let chip_job_id = self.chip_jobs.insert(new_task.clone());
        let old_task = self.current_task.replace(new_task.clone());
        match task_to_job_full(&new_task, chip_job_id) {
            Ok(job_data) => {
                if let Err(e) = self.chip_commands.send(JobCommand::JobFull(job_data)).await {
                    error!(error = ?e, "Failed to send first JobFull to chip");
                    let err = anyhow!("failed to send job to chip: {e:?}");
                    response_tx.send(Err(err)).ok();
                    return ControlFlow::Continue(());
                } else if replace {
                    debug!("Sent first job to chip (old work invalidated)");
                } else {
                    debug!("Sent first job to chip");
                }
            }
            Err(e) => {
                error!(error = %e, "Failed to convert task to JobFull");
                response_tx.send(Err(e)).ok();
                return ControlFlow::Continue(());
            }
        }

        self.set_active(true);
        response_tx.send(Ok(old_task)).ok();
        ControlFlow::Continue(())
    }

    /// Initializes the chain on the first call; later calls are
    /// no-ops.
    async fn ensure_chain_initialized(
        &mut self,
        register_responses: &mut mpsc::Receiver<RegisterResponse>,
    ) -> Result<()> {
        if self.chain_initialized {
            return Ok(());
        }

        trace!("Initializing chain on first assignment.");
        match &self.bring_up_override {
            Some(bring_up) => {
                initialize_chip_bm1366_chain(
                    bring_up.chip_count,
                    bring_up.domain_config,
                    bring_up.baud_control.as_deref(),
                    &mut self.chip_commands,
                    &mut self.peripherals,
                    self.asic_difficulty,
                )
                .await?;
            }
            None => {
                self.initialize_chain(register_responses).await?;
            }
        }
        self.chain_initialized = true;
        Ok(())
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

/// Initialize a chain of `chip_count` BM1366 chips (Antminer S19K Pro
/// AM3-style boards) for mining.
///
/// The sequence is real, captured wire data (via a ptrace syscall
/// tracer against `bosminer`'s own successful bring-up -- see
/// `docs/s19k-pro/reference/full-engineering-log.md`'s Round 4/5/6),
/// not a guessed-at adaptation of any other chip's sequence -- BM1366's
/// actual required order and register values differ genuinely, not
/// just in tuning constants. See [`Bm1366ChainBringUp`]'s doc comment
/// for why this stays a standalone function instead of new
/// `ChipConfig` fields on the shared `Actor::initialize_chain` path.
///
/// **This sequence has mined, but does not out of the box today.**
/// Real `Nonce` responses decoded and the pool accepted real shares in
/// Round 12, and Round 14 confirmed ~4-6 TH/s at ~300 MHz. A fresh
/// build run on real hardware on 2026-08-27 completed the whole
/// sequence below --- all 231 chips addressed, per-chip pass sent,
/// baud switched, chains enabled, the ramp below run to its 575 MHz
/// target, pool connected --- and then produced **zero nonces in five
/// minutes, with board temperature flat at ambient**.
///
/// That is the retest the notes had been calling for, and it came back
/// negative: the `Core`/`CoreMailbox` byte-order fix is present in
/// this crate and is **not on its own sufficient** to make this driver
/// hash at 575 MHz. The signature is the one Round 14 recorded for
/// every frequency above 300 MHz. Whatever else the reference port
/// does differently has not been identified yet.
///
/// What is *not* established for this driver is frequency headroom.
/// Every frequency above ~300 MHz failed identically here -- flat
/// temperature, zero nonces -- and Round 15 found why: a byte-order bug
/// on the `Core`/`CoreMailbox` register left it decoding little-endian
/// where the wire encoding is big-endian, so `CORE_MAILBOX`'s "apply
/// to all cores" bit never applied chain-wide. `CoreCommand::decode`
/// in `register.rs` already decodes big-endian, so that fix carries
/// forward automatically here -- but the ~105 TH/s at 575MHz that
/// proved the fix mattered was measured against the *reference* port,
/// not this driver, which has not been retested above 300 MHz since.
/// Any shortfall now is a driver bug, not a hardware limit.
///
/// One naming correction made while porting this to the current
/// register set: the broadcast write this function always called
/// "NonceRange" (captured at wire address `0x10`) decodes to
/// `RegisterAddress::HashCountingNumber` here, not a `NonceRange`
/// register -- there is no such register in this codebase's more
/// carefully cross-referenced (against real firmware source) register
/// map. The exact captured bytes (`0x5a10_0000`) are unchanged; only
/// the type/name sending them is now the real one.
///
/// Specifics worth knowing before changing any of this:
/// - Switches to `bosminer`'s real 3,125,000 operating baud when the
///   caller supplies a [`BaudControl`], which the S19K Pro board driver
///   does. Round 8 tested the switch on real hardware: it changed
///   nothing by itself, but the investigation found and removed a real
///   corruption source. Uses the exact captured `UartBaud` wire value
///   (`0x00003011`), not this module's `UartBaud::for_baud`, which
///   computes from a target rate and was not verified to reproduce
///   this exact captured value. With no `BaudControl`, stays at the
///   discovery-time 115200 baud throughout.
/// - Sends the per-chip addressed `SoftResetControl`/`MiscControl`/
///   `CoreMailbox` pass with the real captured values, which genuinely
///   differ from the broadcast ones (`00 07 01 f0` / `f0 00 c1 00`,
///   and the `CoreMailbox` triplet in its captured order). Round 10
///   sent this addressed but reused the broadcast-phase data verbatim,
///   since this board's real per-chip values were unknown/uncaptured
///   at the time -- that round shipped and tested clean, but still
///   produced zero Nonce responses. Round 12 went back to the original
///   bosminer wire capture and searched it specifically for
///   *addressed* (non-broadcast) writes -- a query never run before.
///   It turns out the real per-chip values genuinely differ from the
///   broadcast ones, and the real CoreMailbox triplet order is
///   different from what Round 10 guessed. Confirmed identical across
///   all 77 chip addresses (0x00..=0x98) on ttyS1 -- these are fixed
///   values applied to every chip, not per-address-varying data.
async fn initialize_chip_bm1366_chain<W>(
    chip_count: u8,
    domain_config: &[DomainConfigWrite],
    baud_control: Option<&dyn BaudControl>,
    chip_commands: &mut W,
    peripherals: &mut BoardPeripherals,
    asic_difficulty: Log2Difficulty,
) -> Result<()>
where
    W: ChipCommandSink + Unpin,
    SinkError<W>: std::error::Error + Send + Sync + 'static,
{
    debug!("Releasing chip reset");
    peripherals
        .reset_line
        .release()
        .await
        .context("failed to release chip reset")?;

    time::sleep(Duration::from_millis(200)).await;

    async fn send_broadcast<W>(chip_commands: &mut W, register: Register) -> Result<()>
    where
        W: ChipCommandSink + Unpin,
        SinkError<W>: std::error::Error + Send + Sync + 'static,
    {
        chip_commands
            .send(RegisterCommand::WriteRegister(WriteRegister {
                destination: Destination::Broadcast,
                register,
            }))
            .await
            .context("failed to send broadcast register write")
    }

    debug!("Configuring version mask");
    for _ in 0..3 {
        send_broadcast(
            chip_commands,
            Register::MidstateConfig(MidstateConfig::full_rolling()),
        )
        .await
        .context("failed to send MidstateConfig")?;
        time::sleep(Duration::from_millis(15)).await;
    }

    send_broadcast(
        chip_commands,
        Register::SoftResetControl(SoftResetControl::decode([0x00, 0x07, 0x00, 0x00])),
    )
    .await
    .context("failed to send SoftResetControl")?;
    time::sleep(Duration::from_millis(15)).await;

    send_broadcast(
        chip_commands,
        Register::MiscControl(MiscControl::decode([0xff, 0x0f, 0xc1, 0x00])),
    )
    .await
    .context("failed to send MiscControl")?;
    time::sleep(Duration::from_millis(15)).await;

    debug!("Sending ChainInactive");
    for _ in 0..3 {
        chip_commands
            .send(RegisterCommand::ChainInactive(ChainInactive))
            .await
            .context("failed to send ChainInactive")?;
        time::sleep(Duration::from_millis(15)).await;
    }

    debug!(chip_count, "Sweeping SetChipAddress");
    for i in 0..chip_count as u16 {
        let chip_address = (i * 2) as u8;
        chip_commands
            .send(RegisterCommand::SetChipAddress(SetChipAddress {
                chip_address,
            }))
            .await
            .context("failed to send SetChipAddress")?;
        time::sleep(Duration::from_millis(2)).await;
    }

    debug!("Sending broadcast core configuration (CoreMailbox)");
    send_broadcast(
        chip_commands,
        Register::CoreMailbox(CoreCommand::decode([0x80, 0x00, 0x85, 0x40])),
    )
    .await?;
    send_broadcast(
        chip_commands,
        Register::CoreMailbox(CoreCommand::decode([0x80, 0x00, 0x80, 0x20])),
    )
    .await?;
    send_broadcast(
        chip_commands,
        Register::AnalogMux(AnalogMux::decode([0x00, 0x00, 0x00, 0x03])),
    )
    .await?;
    send_broadcast(
        chip_commands,
        Register::IoDriverStrength(IoDriverStrength::decode([0x02, 0x11, 0x41, 0x11])),
    )
    .await?;

    debug!(
        chip_count,
        "Sending per-chip SoftResetControl/MiscControl/CoreMailbox pass"
    );
    for i in 0..chip_count as u16 {
        let chip_address = (i * 2) as u8;
        let destination = Destination::Chip(chip_address);
        chip_commands
            .send(RegisterCommand::WriteRegister(WriteRegister {
                destination,
                register: Register::SoftResetControl(SoftResetControl::decode([
                    0x00, 0x07, 0x01, 0xf0,
                ])),
            }))
            .await
            .context("failed to send per-chip SoftResetControl")?;
        time::sleep(Duration::from_millis(2)).await;
        chip_commands
            .send(RegisterCommand::WriteRegister(WriteRegister {
                destination,
                register: Register::MiscControl(MiscControl::decode([0xf0, 0x00, 0xc1, 0x00])),
            }))
            .await
            .context("failed to send per-chip MiscControl")?;
        time::sleep(Duration::from_millis(2)).await;
        for data in [
            [0x80u8, 0x00, 0x80, 0x20], // reg 0x00 clock delay = 0x20
            [0x80, 0x00, 0x82, 0xaa],   // reg 0x02 core enable = 0xAA
            [0x80, 0x00, 0x85, 0x40],   // reg 0x05 clock select = 0x40
        ] {
            chip_commands
                .send(RegisterCommand::WriteRegister(WriteRegister {
                    destination,
                    register: Register::CoreMailbox(CoreCommand::decode(data)),
                }))
                .await
                .context("failed to send per-chip CoreMailbox write")?;
            time::sleep(Duration::from_millis(2)).await;
        }
    }

    debug!(
        writes = domain_config.len(),
        "Sending per-chip domain config writes"
    );
    for &(chip_address, register_address, data) in domain_config {
        let register = Register::decode(register_address, data)
            .context("failed to decode domain config register")?;
        chip_commands
            .send(RegisterCommand::WriteRegister(WriteRegister {
                destination: Destination::Chip(chip_address),
                register,
            }))
            .await
            .context("failed to send domain config write")?;
        time::sleep(Duration::from_millis(2)).await;
    }

    // Rounds 12-14 (see the engineering log) tried five real
    // hypotheses for why nothing above ~300MHz produced real hashing
    // (flat temperature, zero nonces despite clean communication) --
    // fb_div range, ramp granularity, VCO stability mid-ramp, settle
    // time, post-divider preservation -- all ruled out. A real,
    // independently-developed reference (github.com/Schnitzel/mujina,
    // amlogic-s19kpro-support branch, with its own real hardware
    // testing on this exact BHB56902/S19K Pro hashboard) found two
    // real bugs and one real structural difference:
    //
    // 1. BM1366 requires strict `post_div1 > post_div2`, not `>=`.
    //    Sourced from bitaxeorg/ESP-Miner's real firmware
    //    (`components/asic/bm1366.c`'s `pll_get_parameters`), not a
    //    guess -- see `calculate_pll_bm1366` below, which fixes it.
    // 2. This exact hashboard's real factory operating voltage is
    //    13.9V -- see `PSU_TARGET_VOLTS` in `board/antminer_s19k_am3.rs`.
    // 3. **The frequency ramp runs last**, after TicketMask/the
    //    HashCountingNumber write and the baud switch, not before
    //    them.
    //
    // TicketMask, the HashCountingNumber write, and the baud switch
    // now run immediately after the per-chip pass and domain config,
    // before frequency ramping -- matching the reference's real,
    // working order.
    let ticket_mask = TicketMask::new(asic_difficulty);
    send_broadcast(chip_commands, Register::TicketMask(ticket_mask))
        .await
        .context("failed to send TicketMask")?;

    // Real BM1366 value at wire address 0x10, captured from a working
    // reference bring-up. See this function's doc comment for why this
    // is sent as HashCountingNumber, not a "NonceRange" register.
    send_broadcast(
        chip_commands,
        Register::HashCountingNumber(HashCountingNumber::from(0x5a10_0000u32)),
    )
    .await
    .context("failed to send HashCountingNumber")?;

    time::sleep(Duration::from_millis(150)).await;

    // Switch to bosminer's real confirmed operating baud (3,125,000),
    // if the board supports it. Two things have to happen in order:
    // tell the chips via the real captured UartBaud value first (NOT
    // this module's UartBaud::for_baud, which was not verified to
    // reproduce this exact captured value), *then* reconfigure the
    // host tty to match -- reversing the order would leave the host
    // talking at the old rate to chips already listening at the new
    // one.
    if let Some(baud_control) = baud_control {
        const REAL_OPERATING_BAUD: u32 = 3_125_000;

        debug!(
            baud = REAL_OPERATING_BAUD,
            "Switching to real operating baud"
        );
        send_broadcast(
            chip_commands,
            Register::UartBaud(UartBaud::decode([0x00, 0x00, 0x30, 0x11])),
        )
        .await
        .context("failed to send UartBaud")?;
        time::sleep(Duration::from_millis(50)).await;

        baud_control
            .set_baud_rate(REAL_OPERATING_BAUD)
            .context("failed to switch host tty to real operating baud")?;
        time::sleep(Duration::from_millis(50)).await;
    }

    // Frequency ramp -- deliberately last. Targets 575MHz: the
    // reference's own real hardware measurement on this exact chip
    // family (39.68 TH/s on mujina itself, LuxOS gets 39.15-39.33
    // TH/s at the same point) -- deliberately not the 645MHz factory
    // ceiling, which their notes call "right at the edge of
    // stability" (645MHz measured *less* hashrate than 575MHz).
    const TARGET_MHZ: f32 = 575.0;
    const RAMP_STEP_MHZ: f32 = 6.25;
    const RAMP_STEP_DELAY: Duration = Duration::from_millis(100);

    debug!(
        target_mhz = TARGET_MHZ,
        step_mhz = RAMP_STEP_MHZ,
        "Ramping frequency"
    );
    let mut mhz = 56.25f32;
    loop {
        mhz = if mhz + RAMP_STEP_MHZ < TARGET_MHZ {
            mhz + RAMP_STEP_MHZ
        } else {
            TARGET_MHZ
        };
        let pll = calculate_pll_bm1366(mhz)
            .with_context(|| format!("no valid BM1366 PLL config for {mhz}MHz"))?;
        send_broadcast(chip_commands, Register::PllDivider(pll))
            .await
            .with_context(|| format!("failed to send PllDivider for {mhz}MHz ramp step"))?;
        time::sleep(RAMP_STEP_DELAY).await;
        if mhz >= TARGET_MHZ {
            break;
        }
    }
    debug!(mhz, "Frequency ramp complete");

    Ok(())
}

/// Calculate a PLL configuration for a target frequency, for BM1366
/// (Antminer S19K Pro). Kept standalone rather than added to
/// `ChipConfig::calculate_pll`'s shared search (see
/// `chip_config.rs`'s `PllParams` doc comment: post-divider ordering
/// is "shared across the family and hardcoded in the search loop") --
/// the constraint below is stricter than that shared loop enforces,
/// and changing the shared loop for one still-unverified chip model
/// risks silently changing PLL output for the models that already
/// work. Two things differ from a naive port of that search:
///
/// - fb_div range `0x90..=0xEB` (REFERENCE.md's PLL_DIVIDER section
///   for BM1366), not another model's range.
/// - **Strict `post_div1 > post_div2`**, not `>=`. Sourced from
///   bitaxeorg/ESP-Miner's real firmware (`components/asic/bm1366.c`,
///   `pll_get_parameters(target, 144, 235, ...)`) via a real,
///   independently-developed reference for this exact hashboard
///   (github.com/Schnitzel/mujina, amlogic-s19kpro-support branch) --
///   this is the bug that made every Round 14 hypothesis fail:
///   `post_div1 == post_div2` (e.g. 6x6=36) is a real, electrically
///   invalid combination for this chip family that the non-strict
///   search happily produced.
///
/// Prefers the lowest-VCO valid solution when multiple exist, same
/// rationale as the reference: keeps the VCO in BM1366's typical
/// ~2000-2300MHz operating range rather than an arbitrary higher one.
fn calculate_pll_bm1366(target_freq: f32) -> Option<PllDivider> {
    const CRYSTAL_FREQ: f32 = 25.0;
    const MAX_FREQ_ERROR: f32 = 1.0;
    const FB_DIV_MIN: u8 = 0x90;
    const FB_DIV_MAX: u8 = 0xeb;

    let mut best: Option<(u8, u8, u8, u8)> = None;
    let mut min_error = MAX_FREQ_ERROR;
    let mut best_vco = f32::MAX;

    for ref_div in [2u8, 1] {
        for post_div1 in (1u8..=7).rev() {
            for post_div2 in (1u8..post_div1).rev() {
                let fb_div_f =
                    (post_div1 * post_div2) as f32 * target_freq * ref_div as f32 / CRYSTAL_FREQ;
                let fb_div = fb_div_f.round() as u8;
                if !(FB_DIV_MIN..=FB_DIV_MAX).contains(&fb_div) {
                    continue;
                }
                let actual_freq =
                    CRYSTAL_FREQ * fb_div as f32 / (ref_div * post_div1 * post_div2) as f32;
                let error = (actual_freq - target_freq).abs();
                if error > MAX_FREQ_ERROR {
                    continue;
                }
                let vco = fb_div as f32 * CRYSTAL_FREQ / ref_div as f32;
                if error < min_error || (error <= min_error && vco < best_vco) {
                    min_error = error.min(min_error);
                    best_vco = vco;
                    best = Some((fb_div, ref_div, post_div1, post_div2));
                }
            }
        }
    }

    let (fb_div, ref_div, post_div1, post_div2) = best?;
    let post_div = ((post_div1 - 1) << 4) | (post_div2 - 1);
    Some(PllDivider::new(fb_div, ref_div, post_div))
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
    use std::io;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::{Context, Poll};

    use futures::{Sink, stream};

    use super::*;
    use crate::asic::bm13xx::chip_config;
    use crate::asic::bm13xx::peripherals::ResetLine;
    use crate::peripheral::regulator::VoltageRegulator;
    use crate::types::Voltage;

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

    /// Sink that accepts and discards every command.
    struct NullSink;

    impl Sink<RegisterCommand> for NullSink {
        type Error = io::Error;

        fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
            Poll::Ready(Ok(()))
        }

        fn start_send(self: Pin<&mut Self>, _: RegisterCommand) -> Result<(), io::Error> {
            Ok(())
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    impl Sink<JobCommand> for NullSink {
        type Error = io::Error;

        fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
            Poll::Ready(Ok(()))
        }

        fn start_send(self: Pin<&mut Self>, _: JobCommand) -> Result<(), io::Error> {
            Ok(())
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    /// Sink that is never ready, parking the first send forever.
    struct StallSink;

    impl Sink<RegisterCommand> for StallSink {
        type Error = io::Error;

        fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
            Poll::Pending
        }

        fn start_send(self: Pin<&mut Self>, _: RegisterCommand) -> Result<(), io::Error> {
            unreachable!("poll_ready never succeeds")
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
            Poll::Pending
        }

        fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
            Poll::Pending
        }
    }

    impl Sink<JobCommand> for StallSink {
        type Error = io::Error;

        fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
            Poll::Pending
        }

        fn start_send(self: Pin<&mut Self>, _: JobCommand) -> Result<(), io::Error> {
            unreachable!("poll_ready never succeeds")
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
            Poll::Pending
        }

        fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
            Poll::Pending
        }
    }

    /// Peripheral states observed by the tests.
    #[derive(Clone, Default)]
    struct PeripheralFlags {
        reset_asserted: Arc<AtomicBool>,
        rail_disabled: Arc<AtomicBool>,
    }

    struct MockResetLine(PeripheralFlags);

    #[async_trait]
    impl ResetLine for MockResetLine {
        async fn assert(&mut self) -> Result<()> {
            self.0.reset_asserted.store(true, Ordering::SeqCst);
            Ok(())
        }

        async fn release(&mut self) -> Result<()> {
            self.0.reset_asserted.store(false, Ordering::SeqCst);
            Ok(())
        }
    }

    struct MockRegulator(PeripheralFlags);

    #[async_trait]
    impl VoltageRegulator for MockRegulator {
        async fn enable(&mut self) -> Result<()> {
            self.0.rail_disabled.store(false, Ordering::SeqCst);
            Ok(())
        }

        async fn disable(&mut self) -> Result<()> {
            self.0.rail_disabled.store(true, Ordering::SeqCst);
            Ok(())
        }

        async fn is_enabled(&mut self) -> Result<bool> {
            Ok(!self.0.rail_disabled.load(Ordering::SeqCst))
        }

        async fn set_voltage(&mut self, _voltage: Voltage) -> Result<()> {
            Ok(())
        }

        async fn get_voltage(&mut self) -> Result<Voltage> {
            Ok(Voltage::from_volts(1.15))
        }
    }

    fn spawn_thread<R, W>(
        chip_responses: R,
        chip_commands: W,
    ) -> (BM13xxThread, watch::Sender<()>, PeripheralFlags)
    where
        R: Stream<Item = Result<Response, std::io::Error>> + Unpin + Send + 'static,
        W: ChipCommandSink + Unpin + Send + 'static,
        SinkError<W>: std::error::Error + Send + Sync + 'static,
    {
        let flags = PeripheralFlags::default();
        let peripherals = BoardPeripherals {
            reset_line: Box::new(MockResetLine(flags.clone())),
            voltage_regulator: Box::new(MockRegulator(flags.clone())),
        };
        let (shutdown_tx, shutdown_rx) = watch::channel(());

        let thread = BM13xxThread::new(
            "test".into(),
            chip_config::bm1370(),
            TopologySpec::single_domain(1),
            chip_responses,
            chip_commands,
            peripherals,
            shutdown_rx,
            None,
        );

        (thread, shutdown_tx, flags)
    }

    /// Builds a minimal task for driving the actor.
    fn test_task() -> HashTask {
        use crate::asic::bm13xx::test_data::esp_miner_job;
        use crate::job_source::{
            Extranonce2, GeneralPurposeBits, JobTemplate, MerkleRootKind, VersionTemplate,
        };

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
        let (share_tx, _share_rx) = mpsc::channel(1);

        HashTask {
            template,
            en2_range: None,
            en2: Some(Extranonce2::new(0, 1).unwrap()),
            share_target: crate::types::Difficulty::from(100_u64).to_target(),
            ntime: *esp_miner_job::wire_tx::NTIME,
            share_tx,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn exits_when_response_stream_ends() {
        let (mut thread, _shutdown_tx, flags) = spawn_thread(stream::iter(Vec::new()), NullSink);
        let mut events = thread.take_event_receiver().unwrap();

        // The actor disables the chain before it drops its event
        // sender, so a closed event channel implies the cleanup ran
        let closed = time::timeout(Duration::from_secs(5), events.recv()).await;
        assert!(closed.expect("actor should exit").is_none());

        assert!(flags.reset_asserted.load(Ordering::SeqCst));
        assert!(flags.rail_disabled.load(Ordering::SeqCst));
    }

    #[tokio::test(start_paused = true)]
    async fn exits_when_shutdown_sender_drops() {
        let (mut thread, shutdown_tx, flags) = spawn_thread(
            stream::pending::<Result<Response, std::io::Error>>(),
            NullSink,
        );
        let mut events = thread.take_event_receiver().unwrap();

        // Drop the sender without a signal, as a dying board task
        // would
        drop(shutdown_tx);

        let closed = time::timeout(Duration::from_secs(5), events.recv()).await;
        assert!(closed.expect("actor should exit").is_none());

        assert!(flags.reset_asserted.load(Ordering::SeqCst));
        assert!(flags.rail_disabled.load(Ordering::SeqCst));
    }

    #[tokio::test(start_paused = true)]
    async fn exits_on_shutdown_signal() {
        let (_thread, shutdown_tx, flags) = spawn_thread(
            stream::pending::<Result<Response, std::io::Error>>(),
            NullSink,
        );

        shutdown_tx.send(()).unwrap();

        // The actor drops its shutdown receiver only after disabling
        // the chain, so a closed sender implies the cleanup ran
        time::timeout(Duration::from_secs(5), shutdown_tx.closed())
            .await
            .expect("actor should drop its shutdown receiver");

        assert!(flags.reset_asserted.load(Ordering::SeqCst));
        assert!(flags.rail_disabled.load(Ordering::SeqCst));
    }

    #[tokio::test(start_paused = true)]
    async fn exits_when_handle_dropped() {
        let (mut thread, _shutdown_tx, flags) = spawn_thread(
            stream::pending::<Result<Response, std::io::Error>>(),
            NullSink,
        );
        let mut events = thread.take_event_receiver().unwrap();

        drop(thread);

        let closed = time::timeout(Duration::from_secs(5), events.recv()).await;
        assert!(closed.expect("actor should exit").is_none());

        assert!(flags.reset_asserted.load(Ordering::SeqCst));
        assert!(flags.rail_disabled.load(Ordering::SeqCst));
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_aborts_bring_up() {
        // The stalling sink parks bring-up at its first chip
        // command, holding it in progress until the signal arrives
        let (mut thread, shutdown_tx, flags) = spawn_thread(
            stream::pending::<Result<Response, std::io::Error>>(),
            StallSink,
        );

        let assign = thread.update_task(test_task());
        let signal = async {
            // Bring-up has begun once the startup reset assertion
            // is released
            while !flags.reset_asserted.load(Ordering::SeqCst) {
                time::sleep(Duration::from_millis(1)).await;
            }
            while flags.reset_asserted.load(Ordering::SeqCst) {
                time::sleep(Duration::from_millis(1)).await;
            }
            shutdown_tx.send(()).unwrap();
        };
        let (result, _) = time::timeout(Duration::from_secs(60), async {
            tokio::join!(assign, signal)
        })
        .await
        .expect("bring-up should abort");

        assert!(result.is_err());

        // The actor exits, disabling the chain on the way out
        time::timeout(Duration::from_secs(5), shutdown_tx.closed())
            .await
            .expect("actor should drop its shutdown receiver");
        assert!(flags.reset_asserted.load(Ordering::SeqCst));
        assert!(flags.rail_disabled.load(Ordering::SeqCst));
    }
}
