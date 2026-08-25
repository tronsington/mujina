//! Antminer S19K Pro (AM3/Amlogic control board) support.
//!
//! Unlike `bitaxe.rs`/`emberone00.rs`, this board has no USB
//! co-processor -- it exposes GPIO/I2C directly from the Amlogic SoC
//! to the host Linux kernel, so it registers via
//! [`VirtualBoardDescriptor`] instead of matching a USB device, and
//! its `hw_trait` implementations come from [`crate::linux_hw`]
//! instead of a tunneled management protocol.
//!
//! Full hardware map, GPIO/I2C addresses, and how each was confirmed:
//! `s19k-pro-am3-hardware-notes.md` in the recon docs.
//!
//! **Chip discovery works** (see `HANDOFF.md`'s "Round 5") -- the
//! chips actually self-report as BM1366, not BM1362 as assumed
//! earlier in the investigation. This driver powers the PSU, resets
//! all three chains together (a real hardware requirement found in
//! Round 5: a single chain enabled alone never responds, even at
//! correct voltage), and runs the corrected per-chain bring-up
//! sequence, logging the discovered chip count per chain. **Hash
//! threads are not wired up yet** -- that needs a real BM1366
//! register-init/frequency-tuning sequence (untested beyond
//! discovery) and `BM13xxThread` integration, both still open (see
//! HANDOFF.md's Next Steps). This driver still returns no hash
//! threads, but for a new reason: not because chips are unreachable,
//! but because there's no verified mining-ready register sequence for
//! them yet.

use std::time::Duration;

use anyhow::{Context, Result};
use futures::sink::SinkExt;
use tokio::sync::watch;
use tokio::time::{self, Instant, MissedTickBehavior};
use tokio_stream::StreamExt;
use tokio_util::codec::{FramedRead, FramedWrite};
use tokio_util::sync::CancellationToken;

use super::{BackplaneConnector, BoardInfo, VirtualBoardDescriptor};
use crate::{
    api_client::types::{BoardTelemetry, PowerMeasurement, TemperatureSensor},
    asic::{
        ChipInfo,
        bm13xx::{self, BM13xxProtocol, protocol::Command},
    },
    hw_trait::gpio::{Gpio, GpioPin, PinMode, PinValue},
    linux_hw::{BitBangI2c, LinuxI2c, SysfsGpio, SysfsGpioPin},
    peripheral::{apw12::Apw12, tmp1075::Tmp1075},
    tracing::prelude::*,
    transport::serial::SerialStream,
};

inventory::submit! {
    VirtualBoardDescriptor {
        device_type: "antminer_s19k_am3",
        name: "Antminer S19K Pro (AM3)",
        create_fn: || Box::pin(create_board()),
    }
}

/// `periphs-banks` GPIO controller base line number.
const GPIO_BASE: u32 = 411;

/// Chain enable/reset offsets (sysfs GPIO 454/455/456 - base 411),
/// one per hashboard, confirmed by watching `bosminer`'s own
/// graceful stop/start. Index order matches `CHAIN_TTYS` below
/// (chain 1/GPIO 454/ttyS1, chain 2/GPIO 455/ttyS2, chain 3/GPIO
/// 456/ttyS3).
const CHAIN_ENABLE_OFFSETS: [u8; 3] = [43, 44, 45];

/// Chain presence-detect offsets (sysfs GPIO 439/440/441 - base 411).
const CHAIN_PRESENCE_OFFSETS: [u8; 3] = [28, 29, 30];

/// Per-chain data UARTs, confirmed via a ptrace capture of a real
/// `bosminer` bring-up (HANDOFF.md's "Round 4") and cross-checked
/// live against each chain's own `/proc/<pid>/fd` -- not `ttyS0`, and
/// not a naive "chain N -> ttySN" assumption taken on faith.
const CHAIN_TTYS: [&str; 3] = ["/dev/ttyS1", "/dev/ttyS2", "/dev/ttyS3"];

/// Confirmed real power-on/discovery baud (HANDOFF.md's "Round 5" --
/// verified directly via `TCSETS` ioctl decoding, not assumed).
const CHAIN_BAUD: u32 = 115_200;

/// PSU output enable, active-low (`PSU_nEN`, sysfs GPIO 437 - base
/// 411 = offset 26). Not reachable via the PSU's own I2C protocol --
/// a separate physical enable line.
const PSU_NEN_OFFSET: u8 = 26;

/// PSU bit-banged I2C bus, labelled `I2C_SCL`/`I2C_SDA` in Bitmain's
/// own `/etc/init.d/S37board_setup` -- not reachable via any real
/// `/dev/i2c-N` device.
const PSU_SCL_GPIO: u32 = 476;
const PSU_SDA_GPIO: u32 = 477;

/// Real target output voltage, confirmed from `bosminer`'s own PSU
/// ramp log (HANDOFF.md's "Round 3").
const PSU_TARGET_VOLTS: f32 = 15.2;
const PSU_RAMP_STEP_VOLTS: f32 = 0.5;
const PSU_RAMP_STEP_DELAY: Duration = Duration::from_millis(1500);

/// TMP75-compatible (LM75BCCn-clone) sensor addresses on the real
/// hardware I2C bus (`/dev/i2c-1`), `(inlet, outlet)` per hashboard.
const TEMP_SENSOR_ADDRS: [(u8, u8); 3] = [(0x48, 0x4C), (0x4D, 0x49), (0x4E, 0x4A)];

const SENSOR_I2C_DEVICE: &str = "/dev/i2c-1";

/// Expected chips per chain (S19K Pro AM3 hashboards).
pub(crate) const EXPECTED_CHIPS: usize = 77;

/// Per-chip addressed `IoDriverStrength`(`0x58`)/`UartRelay`(`0x2C`)
/// writes, captured verbatim from a real successful bring-up (chain
/// 3) via the `s19k-trace` ptrace tracer -- see HANDOFF.md's "Round
/// 4"/"Round 5". Only a subset of chips (roughly every 7th address)
/// get them directly; `UartRelay`'s own doc comment calls it "domain
/// relay configuration", so this is very likely per-domain-boundary
/// config that propagates internally to the rest of each domain, not
/// a per-chip requirement -- but the exact semantics aren't
/// understood yet (see HANDOFF.md's Next Steps), so this replays the
/// real bytes rather than guessing which subset/values matter.
/// (chip_address, register_address, raw wire data bytes). Kept in
/// sync with (not shared code with) the identical table in
/// `src/bin/s19k_probe.rs` -- that tool's raw-byte diagnostics serve
/// a different purpose (is a chain truly silent vs. garbled?) than
/// this driver's clean typed decode, so they're intentionally
/// separate rather than forced through one abstraction.
pub(crate) const DOMAIN_CONFIG_WRITES: &[(u8, bm13xx::protocol::RegisterAddress, [u8; 4])] = {
    use bm13xx::protocol::RegisterAddress::{IoDriverStrength, UartRelay};
    &[
        (0x98, IoDriverStrength, [0x02, 0x11, 0x41, 0x11]),
        (0x8a, IoDriverStrength, [0x02, 0x11, 0x41, 0x11]),
        (0x7c, IoDriverStrength, [0x02, 0x11, 0x41, 0x11]),
        (0x6e, IoDriverStrength, [0x02, 0x11, 0x41, 0x11]),
        (0x60, IoDriverStrength, [0x02, 0x11, 0x41, 0x11]),
        (0x52, IoDriverStrength, [0x02, 0x11, 0x41, 0x11]),
        (0x44, IoDriverStrength, [0x02, 0x11, 0x41, 0x11]),
        (0x36, IoDriverStrength, [0x02, 0x11, 0x41, 0x11]),
        (0x28, IoDriverStrength, [0x02, 0x11, 0x41, 0x11]),
        (0x1a, IoDriverStrength, [0x02, 0x11, 0x41, 0x11]),
        (0x0c, IoDriverStrength, [0x02, 0x11, 0x41, 0x11]),
        (0x8c, UartRelay, [0x00, 0x15, 0x00, 0x03]),
        (0x98, UartRelay, [0x00, 0x15, 0x00, 0x03]),
        (0x7e, UartRelay, [0x00, 0x1c, 0x00, 0x03]),
        (0x8a, UartRelay, [0x00, 0x1c, 0x00, 0x03]),
        (0x70, UartRelay, [0x00, 0x23, 0x00, 0x03]),
        (0x7c, UartRelay, [0x00, 0x23, 0x00, 0x03]),
        (0x62, UartRelay, [0x00, 0x2a, 0x00, 0x03]),
        (0x6e, UartRelay, [0x00, 0x2a, 0x00, 0x03]),
        (0x54, UartRelay, [0x00, 0x31, 0x00, 0x03]),
        (0x60, UartRelay, [0x00, 0x31, 0x00, 0x03]),
        (0x46, UartRelay, [0x00, 0x38, 0x00, 0x03]),
        (0x52, UartRelay, [0x00, 0x38, 0x00, 0x03]),
        (0x38, UartRelay, [0x00, 0x3f, 0x00, 0x03]),
        (0x44, UartRelay, [0x00, 0x3f, 0x00, 0x03]),
        (0x2a, UartRelay, [0x00, 0x46, 0x00, 0x03]),
        (0x36, UartRelay, [0x00, 0x46, 0x00, 0x03]),
        (0x1c, UartRelay, [0x00, 0x4d, 0x00, 0x03]),
        (0x28, UartRelay, [0x00, 0x4d, 0x00, 0x03]),
        (0x0e, UartRelay, [0x00, 0x54, 0x00, 0x03]),
        (0x1a, UartRelay, [0x00, 0x54, 0x00, 0x03]),
        (0x00, UartRelay, [0x00, 0x5b, 0x00, 0x03]),
    ]
};

async fn create_board() -> Result<BackplaneConnector> {
    let mut chain_gpio = SysfsGpio::new(GPIO_BASE);

    // Hold all three chains in reset while power comes up, matching
    // bitaxe.rs's "hold ASIC in reset during power configuration"
    // pattern.
    let mut enable_pins = Vec::with_capacity(CHAIN_ENABLE_OFFSETS.len());
    for &offset in &CHAIN_ENABLE_OFFSETS {
        let mut pin = chain_gpio.pin(offset).await.context("chain enable pin")?;
        pin.set_mode(PinMode::Output).await?;
        pin.write(PinValue::Low).await?;
        enable_pins.push(pin);
    }

    let mut presence = [false; CHAIN_PRESENCE_OFFSETS.len()];
    for (chain, &offset) in CHAIN_PRESENCE_OFFSETS.iter().enumerate() {
        let mut pin = chain_gpio.pin(offset).await.context("presence pin")?;
        pin.set_mode(PinMode::Input).await?;
        presence[chain] = pin.read().await? == PinValue::High;
    }
    info!(?presence, "Hashboard presence detect");

    let mut psu_nen_pin = chain_gpio
        .pin(PSU_NEN_OFFSET)
        .await
        .context("PSU_nEN pin")?;
    psu_nen_pin.set_mode(PinMode::Output).await?;
    psu_nen_pin.write(PinValue::High).await?; // disabled (safe default) until bring-up below

    let psu_i2c = BitBangI2c::new(PSU_SCL_GPIO, PSU_SDA_GPIO);
    let mut psu = Apw12::new(psu_i2c);

    let temp_sensors: Vec<Tmp1075<LinuxI2c>> = TEMP_SENSOR_ADDRS
        .iter()
        .flat_map(|&(inlet, outlet)| [inlet, outlet])
        .map(|addr| {
            let i2c = LinuxI2c::open(SENSOR_I2C_DEVICE)
                .with_context(|| format!("opening {SENSOR_I2C_DEVICE}"))?;
            Ok(Tmp1075::new(i2c, addr))
        })
        .collect::<Result<_>>()?;

    // Power up and discover chips. Deliberately non-fatal: a failure
    // here (PSU I2C glitch, one chain not responding, etc.) shouldn't
    // take down temperature/PSU telemetry, which are independently
    // useful. Real hash threads aren't created either way yet (see
    // module docs), so there's nothing downstream that actually
    // depends on this succeeding today.
    match bring_up_chains(&mut psu, &mut psu_nen_pin, &mut enable_pins).await {
        Ok(chip_counts) => {
            for (chain, count) in chip_counts.iter().enumerate() {
                if *count == EXPECTED_CHIPS {
                    info!(chain = chain + 1, chips = count, "Chain discovery OK");
                } else {
                    warn!(
                        chain = chain + 1,
                        chips = count,
                        expected = EXPECTED_CHIPS,
                        "Chain discovery incomplete"
                    );
                }
            }
        }
        Err(e) => {
            warn!(error = ?e, "Chain bring-up/discovery failed");
        }
    }

    let info = BoardInfo {
        model: "Antminer S19K Pro (AM3)".to_string(),
        firmware_version: None,
        // No EEPROM parser yet (Bitmain's format is undocumented) to
        // derive a real serial from; see hardware notes' EEPROM
        // section for the raw layout if that gets picked up.
        serial_number: Some("antminer-s19k-am3".to_string()),
    };

    let initial_telemetry = BoardTelemetry {
        name: info.serial_number.clone().unwrap(),
        model: info.model.clone(),
        serial: info.serial_number.clone(),
        ..Default::default()
    };
    let (telemetry_tx, telemetry_rx) = watch::channel(initial_telemetry);

    let cancel = CancellationToken::new();
    let monitor_task = spawn_monitor(temp_sensors, psu, telemetry_tx, cancel.clone());

    let mut board = AntminerS19kAm3 {
        enable_pins,
        psu_nen_pin,
        monitor_cancel: cancel,
        monitor_task,
    };

    let shutdown = Box::pin(async move {
        board.shutdown().await;
    });

    warn!(
        "BM1366 register-init/frequency-tuning sequence not yet solved for this board \
         (chip discovery works -- see HANDOFF.md's Round 5) -- registering with no hash threads"
    );

    Ok(BackplaneConnector {
        info,
        threads: Vec::new(),
        telemetry_rx,
        shutdown: Some(shutdown),
    })
}

/// Power the hashboards and discover chips on all three chains.
///
/// Returns the discovered chip count per chain (index-matched to
/// `CHAIN_TTYS`/`CHAIN_ENABLE_OFFSETS`) on success. A chain returning
/// fewer than `EXPECTED_CHIPS` isn't itself an error here -- the
/// caller decides how to log/react; this function's job is just to
/// run the real bring-up and report what happened.
async fn bring_up_chains(
    psu: &mut Apw12<BitBangI2c>,
    psu_nen_pin: &mut SysfsGpioPin,
    enable_pins: &mut [SysfsGpioPin],
) -> Result<[usize; 3]> {
    psu.disable_watchdog()
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("PSU disable_watchdog")?;

    psu_nen_pin
        .write(PinValue::Low)
        .await
        .context("enabling PSU output (PSU_nEN low)")?;

    ramp_psu_voltage(psu, PSU_TARGET_VOLTS).await?;

    // Reset all three chains *together* -- a real hardware
    // requirement found the hard way (HANDOFF.md's "Round 5"): a
    // single chain enabled alone, even at confirmed correct voltage,
    // never responds to anything. All three enabled together does.
    for pin in enable_pins.iter_mut() {
        pin.write(PinValue::Low).await?;
    }
    time::sleep(Duration::from_millis(200)).await;
    for pin in enable_pins.iter_mut() {
        pin.write(PinValue::High).await?;
    }
    // bosminer's own log shows ~2s between "Resetting hash board" and
    // "Initializing hashchain" (its first UART command); matching
    // that gap here in case chips need real settle time after reset.
    time::sleep(Duration::from_millis(2000)).await;

    let mut chip_counts = [0usize; 3];
    for (chain, tty) in CHAIN_TTYS.iter().enumerate() {
        match discover_chain(tty).await {
            Ok(chips) => chip_counts[chain] = chips.len(),
            Err(e) => warn!(chain = chain + 1, tty, error = ?e, "Chain discovery failed"),
        }
    }
    Ok(chip_counts)
}

/// Gradually raise PSU output voltage to `target`, matching
/// `bosminer`'s own observed ramp behavior (HANDOFF.md's "Round 3")
/// rather than jumping straight to a high setpoint on a rail that may
/// not have been driven in a while.
async fn ramp_psu_voltage(psu: &mut Apw12<BitBangI2c>, target: f32) -> Result<()> {
    let mut current = psu
        .get_voltage()
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("reading PSU voltage setpoint")?;

    while current < target {
        current = (current + PSU_RAMP_STEP_VOLTS).min(target);
        set_voltage_with_retries(psu, current).await?;
        time::sleep(PSU_RAMP_STEP_DELAY).await;
    }
    Ok(())
}

/// `set_voltage`, retrying a bounded number of times on failure.
///
/// The bit-banged PSU bus is a real software I2C implementation over
/// two GPIO lines with no hardware framing/clock-stretching support --
/// HANDOFF.md's PSU section already documents occasional flakiness
/// (voltage readings that bounce, attributed to bit-bang timing noise
/// under real system load). A single `set_voltage` from a fresh CLI
/// process is reliable, but a longer in-process ramp (many
/// transactions back to back) occasionally NAKs partway through --
/// consistent with the same documented noise, not a logic error in
/// the ramp itself. Retrying the same value is safe: `set_voltage` is
/// idempotent.
async fn set_voltage_with_retries(psu: &mut Apw12<BitBangI2c>, volts: f32) -> Result<()> {
    const MAX_ATTEMPTS: u32 = 3;
    const RETRY_DELAY: Duration = Duration::from_millis(200);

    for attempt in 1..=MAX_ATTEMPTS {
        match psu.set_voltage(volts).await {
            Ok(()) => return Ok(()),
            Err(e) if attempt < MAX_ATTEMPTS => {
                warn!(volts, attempt, error = %e, "PSU set_voltage failed, retrying");
                time::sleep(RETRY_DELAY).await;
            }
            Err(e) => {
                return Err(anyhow::anyhow!("{e}"))
                    .with_context(|| format!("setting PSU voltage to {volts:.3}V"));
            }
        }
    }
    unreachable!("loop always returns on the final attempt")
}

/// Run the corrected chip discovery sequence (HANDOFF.md's "Round 5")
/// on one chain's UART and return the chips that responded.
///
/// Real captured sequence: VersionMask x3 -> InitControl -> MiscControl
/// -> ChainInactive x3 -> SetChipAddress sweep (one per expected chip)
/// -> per-chip domain config writes -> discover, sent *last* (not
/// first, which is what every earlier round of this investigation had
/// assumed and which never got a single response).
pub(crate) async fn discover_chain(tty: &str) -> Result<Vec<ChipInfo>> {
    use bm13xx::protocol::{Register, RegisterAddress, VersionMask};

    let stream = SerialStream::new(tty, CHAIN_BAUD)
        .map_err(|e| anyhow::anyhow!("{e}"))
        .with_context(|| format!("opening {tty}"))?;
    let (reader, writer, _control) = stream.split();
    let mut reader = FramedRead::new(reader, bm13xx::FrameCodec);
    let mut writer = FramedWrite::new(writer, bm13xx::FrameCodec);

    for _ in 0..3 {
        writer
            .send(Command::WriteRegister {
                broadcast: true,
                chip_address: 0x00,
                register: Register::VersionMask(VersionMask::full_rolling()),
            })
            .await
            .context("sending VersionMask")?;
        time::sleep(Duration::from_millis(15)).await;
    }

    writer
        .send(Command::WriteRegister {
            broadcast: true,
            chip_address: 0x00,
            register: Register::decode(RegisterAddress::InitControl, &[0x00, 0x07, 0x00, 0x00]),
        })
        .await
        .context("sending InitControl")?;
    time::sleep(Duration::from_millis(15)).await;

    writer
        .send(Command::WriteRegister {
            broadcast: true,
            chip_address: 0x00,
            register: Register::decode(RegisterAddress::MiscControl, &[0xff, 0x0f, 0xc1, 0x00]),
        })
        .await
        .context("sending MiscControl")?;
    time::sleep(Duration::from_millis(15)).await;

    for _ in 0..3 {
        writer
            .send(Command::ChainInactive)
            .await
            .context("sending ChainInactive")?;
        time::sleep(Duration::from_millis(15)).await;
    }

    for i in 0..EXPECTED_CHIPS as u16 {
        let chip_address = (i * 2) as u8;
        writer
            .send(Command::SetChipAddress { chip_address })
            .await
            .context("sending SetChipAddress")?;
        time::sleep(Duration::from_millis(2)).await;
    }

    for &(chip_address, register_address, data) in DOMAIN_CONFIG_WRITES {
        let register = Register::decode(register_address, &data);
        writer
            .send(Command::WriteRegister {
                broadcast: false,
                chip_address,
                register,
            })
            .await
            .context("sending domain config write")?;
        time::sleep(Duration::from_millis(2)).await;
    }

    writer
        .send(BM13xxProtocol::discover_chips())
        .await
        .context("sending discover_chips")?;

    let mut chips = Vec::new();
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        tokio::select! {
            response = reader.next() => {
                match response {
                    Some(Ok(bm13xx::Response::ReadRegister {
                        chip_address: _,
                        register: bm13xx::Register::ChipId { chip_type, core_count, address },
                    })) => {
                        chips.push(ChipInfo {
                            chip_id: chip_type.id_bytes(),
                            core_count: core_count.into(),
                            address,
                            supports_version_rolling: true,
                        });
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => warn!(tty, error = %e, "decode error during discovery"),
                    None => break,
                }
            }
            _ = time::sleep_until(deadline) => break,
        }
    }

    Ok(chips)
}

/// Spawn a task that periodically reads sensors and PSU telemetry.
fn spawn_monitor(
    mut temp_sensors: Vec<Tmp1075<LinuxI2c>>,
    mut psu: Apw12<BitBangI2c>,
    telemetry_tx: watch::Sender<BoardTelemetry>,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        const INTERVAL: Duration = Duration::from_secs(5);
        let mut ticker = time::interval(INTERVAL);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        ticker.tick().await; // discard the immediate first tick

        const SENSOR_NAMES: [&str; 6] = [
            "chain1-inlet",
            "chain1-outlet",
            "chain2-inlet",
            "chain2-outlet",
            "chain3-inlet",
            "chain3-outlet",
        ];

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = ticker.tick() => {}
            }

            let mut temperatures = Vec::with_capacity(temp_sensors.len());
            for (sensor, &name) in temp_sensors.iter_mut().zip(SENSOR_NAMES.iter()) {
                let reading = match sensor.read().await {
                    Ok(reading) => Some(reading.into()),
                    Err(e) => {
                        warn!(sensor = name, error = %e, "temperature read failed");
                        None
                    }
                };
                temperatures.push(TemperatureSensor {
                    name: name.to_string(),
                    temperature: reading,
                });
            }

            let power = match psu.measure_voltage().await {
                Ok(voltage) => Some(PowerMeasurement {
                    name: "psu".to_string(),
                    voltage_v: Some(voltage),
                    current_a: None,
                    power_w: None,
                }),
                Err(e) => {
                    warn!(error = %e, "PSU voltage read failed");
                    None
                }
            };

            telemetry_tx.send_modify(|t| {
                t.temperatures = temperatures;
                t.powers = power.into_iter().collect();
            });
        }
    })
}

/// Antminer S19K Pro board state, held for the board's lifetime.
struct AntminerS19kAm3 {
    enable_pins: Vec<SysfsGpioPin>,
    psu_nen_pin: SysfsGpioPin,
    monitor_cancel: CancellationToken,
    monitor_task: tokio::task::JoinHandle<()>,
}

impl AntminerS19kAm3 {
    async fn shutdown(&mut self) {
        self.monitor_cancel.cancel();
        let _ = (&mut self.monitor_task).await;
        for pin in &mut self.enable_pins {
            let _ = pin.write(PinValue::Low).await;
        }
        let _ = self.psu_nen_pin.write(PinValue::High).await; // disable PSU output
    }
}
