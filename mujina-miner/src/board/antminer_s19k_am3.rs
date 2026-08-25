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
//! **BM1362 chip communication is not yet solved** (see
//! `HANDOFF.md`'s chip-discovery investigation) -- like
//! `emberone00.rs` before it finished its own chip's bring-up, this
//! board registers, wires up the peripherals that *are* working
//! (chain presence/enable GPIO, temperature sensors, PSU telemetry),
//! and returns no hash threads. **PSU output is deliberately left
//! disabled by default** -- there is no working chip protocol yet to
//! justify powering the hashboards, so this driver doesn't do it as
//! a side effect of loading. Enabling it is real, deliberate future
//! work once chip bring-up succeeds.

use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::watch;
use tokio::time::{self, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

use super::{BackplaneConnector, BoardInfo, VirtualBoardDescriptor};
use crate::{
    api_client::types::{BoardTelemetry, PowerMeasurement, TemperatureSensor},
    hw_trait::gpio::{Gpio, GpioPin, PinMode, PinValue},
    linux_hw::{BitBangI2c, LinuxI2c, SysfsGpio, SysfsGpioPin},
    peripheral::{apw12::Apw12, tmp1075::Tmp1075},
    tracing::prelude::*,
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
/// graceful stop/start.
const CHAIN_ENABLE_OFFSETS: [u8; 3] = [43, 44, 45];

/// Chain presence-detect offsets (sysfs GPIO 439/440/441 - base 411).
const CHAIN_PRESENCE_OFFSETS: [u8; 3] = [28, 29, 30];

/// PSU bit-banged I2C bus, labelled `I2C_SCL`/`I2C_SDA` in Bitmain's
/// own `/etc/init.d/S37board_setup` -- not reachable via any real
/// `/dev/i2c-N` device.
const PSU_SCL_GPIO: u32 = 476;
const PSU_SDA_GPIO: u32 = 477;

/// TMP75-compatible (LM75BCCn-clone) sensor addresses on the real
/// hardware I2C bus (`/dev/i2c-1`), `(inlet, outlet)` per hashboard.
const TEMP_SENSOR_ADDRS: [(u8, u8); 3] = [(0x48, 0x4C), (0x4D, 0x49), (0x4E, 0x4A)];

const SENSOR_I2C_DEVICE: &str = "/dev/i2c-1";

async fn create_board() -> Result<BackplaneConnector> {
    let mut chain_gpio = SysfsGpio::new(GPIO_BASE);

    // Release the chain reset lines, matching bosminer's own
    // steady-state value. Harmless with PSU output left off (below):
    // no current flows through an unpowered chain regardless of this
    // pin's state.
    let mut enable_pins = Vec::with_capacity(CHAIN_ENABLE_OFFSETS.len());
    for &offset in &CHAIN_ENABLE_OFFSETS {
        let mut pin = chain_gpio.pin(offset).await.context("chain enable pin")?;
        pin.set_mode(PinMode::Output).await?;
        pin.write(PinValue::High).await?;
        enable_pins.push(pin);
    }

    let mut presence = [false; CHAIN_PRESENCE_OFFSETS.len()];
    for (chain, &offset) in CHAIN_PRESENCE_OFFSETS.iter().enumerate() {
        let mut pin = chain_gpio.pin(offset).await.context("presence pin")?;
        pin.set_mode(PinMode::Input).await?;
        presence[chain] = pin.read().await? == PinValue::High;
    }
    info!(?presence, "Hashboard presence detect");

    let psu_i2c = BitBangI2c::new(PSU_SCL_GPIO, PSU_SDA_GPIO);
    let psu = Apw12::new(psu_i2c);

    let temp_sensors: Vec<Tmp1075<LinuxI2c>> = TEMP_SENSOR_ADDRS
        .iter()
        .flat_map(|&(inlet, outlet)| [inlet, outlet])
        .map(|addr| {
            let i2c = LinuxI2c::open(SENSOR_I2C_DEVICE)
                .with_context(|| format!("opening {SENSOR_I2C_DEVICE}"))?;
            Ok(Tmp1075::new(i2c, addr))
        })
        .collect::<Result<_>>()?;

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
        monitor_cancel: cancel,
        monitor_task,
    };

    let shutdown = Box::pin(async move {
        board.shutdown().await;
    });

    warn!(
        "BM1362 chip bring-up not yet solved for this board (see HANDOFF.md) \
         -- registering with no hash threads"
    );

    Ok(BackplaneConnector {
        info,
        threads: Vec::new(),
        telemetry_rx,
        shutdown: Some(shutdown),
    })
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
    }
}
