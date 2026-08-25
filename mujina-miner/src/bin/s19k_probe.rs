//! Standalone probe for the Antminer S19K Pro (Amlogic A113D control
//! board, 3x BM1362 hashboards).
//!
//! Not part of the real board driver yet -- this exists to answer one
//! question empirically before investing in the full backplane
//! integration: does toggling the chain-enable GPIO and speaking
//! Mujina's real BM13xx protocol over the native ttySN UART actually
//! get responses from the physical chips.
//!
//! Usage: s19k-probe <chain 1|2|3> [baud]
//!
//! Chain -> GPIO enable line (periphs-banks controller, base 411) and
//! UART device, per s19k-pro-am3-hardware-notes.md:
//!   chain 1: GPIO 454, /dev/ttyS1
//!   chain 2: GPIO 455, /dev/ttyS2
//!   chain 3: GPIO 456, /dev/ttyS3
//! bosminer's own bring-up ramps baud 115200 -> 9600 -> 115200 ->
//! 3,125,000 -- chips power up listening at a low default baud, not
//! the final high speed. `baud` defaults to 115200 (the power-on
//! default) rather than the final 3,125,000 operating speed.
//! 77 chips expected per chain.

use std::fs;
use std::io;
use std::time::Duration;

use bytes::BytesMut;
use futures::sink::SinkExt;
use mujina_miner::asic::bm13xx::{
    self, BM13xxProtocol,
    protocol::{Command, Log2Difficulty, TicketMask},
};
use mujina_miner::transport::serial::{SerialReader, SerialStream};
use mujina_miner::types::{HashRate, ShareRate};
use tokio::io::AsyncReadExt;
use tokio::time::{self, Instant};
use tokio_util::codec::{Decoder, FramedWrite};

fn chain_gpio(chain: u8) -> u32 {
    // 454/455/456 = per-chain hashboard enable, confirmed by watching
    // bosminer's own graceful stop/start (see hardware notes).
    453 + chain as u32
}

fn chain_tty(chain: u8) -> String {
    format!("/dev/ttyS{}", chain)
}

const EXPECTED_CHIPS: usize = 77;

/// Per-chip addressed IoDriverStrength (register 0x58) / UartRelay
/// (register 0x2C) writes, captured verbatim from a real successful
/// bring-up (chain 3) via the `s19k-trace` ptrace tracer -- see
/// HANDOFF.md's "Round 4". Only a subset of chips (roughly every 7th
/// address) get written directly; UartRelay's own doc comment calls
/// it "domain relay configuration", so this is very likely
/// per-domain-boundary chip config that propagates internally to the
/// rest of each domain, not a per-chip requirement -- but the exact
/// semantics aren't understood yet, so this replays the real bytes
/// rather than guessing which subset/values matter. (chip_address,
/// register_address, raw wire data bytes).
const DOMAIN_CONFIG_WRITES: &[(u8, bm13xx::protocol::RegisterAddress, [u8; 4])] = {
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

fn sysfs_export(gpio: u32) -> io::Result<()> {
    if fs::metadata(format!("/sys/class/gpio/gpio{gpio}")).is_ok() {
        return Ok(());
    }
    match fs::write("/sys/class/gpio/export", gpio.to_string()) {
        Ok(()) => Ok(()),
        // EBUSY: already exported by a racing actor between the
        // metadata check and this write. Fine either way.
        Err(e) if e.raw_os_error() == Some(16) => Ok(()),
        Err(e) => Err(e),
    }
}

fn sysfs_set_output_high(gpio: u32) -> io::Result<()> {
    sysfs_export(gpio)?;
    // The export creates the gpio<N> directory asynchronously in
    // some kernels; give it a moment.
    std::thread::sleep(Duration::from_millis(50));
    fs::write(format!("/sys/class/gpio/gpio{gpio}/direction"), "out")?;
    fs::write(format!("/sys/class/gpio/gpio{gpio}/value"), "1")?;
    Ok(())
}

fn sysfs_set_output_low(gpio: u32) -> io::Result<()> {
    sysfs_export(gpio)?;
    std::thread::sleep(Duration::from_millis(50));
    fs::write(format!("/sys/class/gpio/gpio{gpio}/direction"), "out")?;
    fs::write(format!("/sys/class/gpio/gpio{gpio}/value"), "0")?;
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    mujina_miner::tracing::init();

    let mut args = std::env::args().skip(1);
    let chain: u8 = args
        .next()
        .expect("usage: s19k-probe <chain 1|2|3> [baud] [tty-override, e.g. ttyS2]")
        .parse()
        .expect("chain must be 1, 2, or 3");
    anyhow::ensure!((1..=3).contains(&chain), "chain must be 1, 2, or 3");
    let baud: u32 = args
        .next()
        .map(|s| s.parse().expect("baud must be a number"))
        .unwrap_or(115_200);
    // Override which UART to listen on without changing which GPIO
    // gets enabled -- lets chain-enable and tty-device be
    // cross-matched independently to settle numbering questions.
    let tty_override = args.next();

    let gpio = chain_gpio(chain);
    let tty = tty_override
        .map(|t| format!("/dev/{t}"))
        .unwrap_or_else(|| chain_tty(chain));

    // A real reset pulse (low, then high), not just "set high" from
    // whatever state the GPIO happened to already be in -- bosminer's
    // own log calls this step "Resetting hash board", which implies
    // an actual falling+rising edge, not an idempotent level set.
    // Earlier rounds tried "set low before PSU on, high after" but
    // never combined it with the corrected command order below.
    println!("Resetting chain {chain}: GPIO {gpio} low...");
    sysfs_set_output_low(gpio)?;
    time::sleep(Duration::from_millis(200)).await;
    println!("Releasing reset: GPIO {gpio} high...");
    sysfs_set_output_high(gpio)?;
    // bosminer's own log shows ~2s between "Resetting hash board" and
    // "Initializing hashchain" (its first UART command) -- matching
    // that gap here in case chips need real settle time after reset
    // before they're listening.
    println!("Settling for 2s before UART traffic (matching bosminer's own observed gap)...");
    time::sleep(Duration::from_millis(2000)).await;

    println!("Opening {tty} at {baud} baud...");
    let stream = SerialStream::new(&tty, baud)?;
    let (mut reader, writer, _control) = stream.split();
    let mut data_writer = FramedWrite::new(writer, bm13xx::FrameCodec);

    // Real captured sequence from bosminer's own successful bring-up
    // (via a ptrace syscall tracer -- see HANDOFF.md's "Round 4").
    // Every earlier attempt sent VersionMask then discover_chips() in
    // isolation and got zero responses. The real firmware sends
    // discover LAST, after every chip has already been individually
    // addressed (SetChipAddress sweep) and individually configured
    // (per-chip IoDriverStrength/UartRelay writes) -- not first. This
    // replays that real sequence, byte-for-byte where the exact
    // semantics aren't yet understood (the per-chip domain writes
    // below), reusing typed Command/Register where they line up
    // cleanly with existing register definitions.
    println!("Sending VersionMask (broadcast, x3, matching the real capture)...");
    for _ in 0..3 {
        let version_mask_cmd = Command::WriteRegister {
            broadcast: true,
            chip_address: 0x00,
            register: bm13xx::protocol::Register::VersionMask(
                bm13xx::protocol::VersionMask::full_rolling(),
            ),
        };
        data_writer.send(version_mask_cmd).await?;
        time::sleep(Duration::from_millis(15)).await;
    }

    println!("Sending InitControl (0xA8, broadcast, raw_value=0x00000700)...");
    data_writer
        .send(Command::WriteRegister {
            broadcast: true,
            chip_address: 0x00,
            register: bm13xx::protocol::Register::decode(
                bm13xx::protocol::RegisterAddress::InitControl,
                &[0x00, 0x07, 0x00, 0x00],
            ),
        })
        .await?;
    time::sleep(Duration::from_millis(15)).await;

    println!("Sending MiscControl (0x18, broadcast, raw_value=0x00c10fff)...");
    data_writer
        .send(Command::WriteRegister {
            broadcast: true,
            chip_address: 0x00,
            register: bm13xx::protocol::Register::decode(
                bm13xx::protocol::RegisterAddress::MiscControl,
                &[0xff, 0x0f, 0xc1, 0x00],
            ),
        })
        .await?;
    time::sleep(Duration::from_millis(15)).await;

    println!("Sending ChainInactive (broadcast, x3) -- enables daisy-chain addressing...");
    for _ in 0..3 {
        data_writer.send(Command::ChainInactive).await?;
        time::sleep(Duration::from_millis(15)).await;
    }

    println!(
        "Sending SetChipAddress sweep: {EXPECTED_CHIPS} chips, address 0x00 step 2 up to {:#04x}...",
        (EXPECTED_CHIPS as u16 - 1) * 2
    );
    for i in 0..EXPECTED_CHIPS as u16 {
        let chip_address = (i * 2) as u8;
        data_writer
            .send(Command::SetChipAddress { chip_address })
            .await?;
        time::sleep(Duration::from_millis(2)).await;
    }

    println!(
        "Sending {} per-chip domain config writes (IoDriverStrength/UartRelay, replayed verbatim from the real capture -- semantics not yet understood)...",
        DOMAIN_CONFIG_WRITES.len()
    );
    for &(chip_address, register_address, data) in DOMAIN_CONFIG_WRITES {
        let register = bm13xx::protocol::Register::decode(register_address, &data);
        data_writer
            .send(Command::WriteRegister {
                broadcast: false,
                chip_address,
                register,
            })
            .await?;
        time::sleep(Duration::from_millis(2)).await;
    }

    println!(
        "Sending discover_chips (broadcast ChipId read) -- as the LAST step, matching the real capture..."
    );
    data_writer.send(BM13xxProtocol::discover_chips()).await?;

    let (chips, total_bytes) = read_responses(&mut reader, Duration::from_millis(2000)).await?;
    report(chain, "initial discovery", &chips, total_bytes);

    // HANDOFF.md's Round 7/8: real hardware testing found chips accept
    // jobs but never return a single Nonce. A first pass sending the
    // *whole* post-discovery config sequence then re-discovering
    // found a real regression (77 -> 54 chips, and a flood of garbled
    // bytes rather than silence) -- so *something* in this sequence
    // corrupts communication. This version isolates which single step
    // does it: send one register write, then immediately re-discover,
    // for each step in turn, so the exact culprit shows up as the
    // first re-discovery that regresses.
    let asic_difficulty = Log2Difficulty::from_difficulty(
        ShareRate::per_second(1.0).to_difficulty(HashRate::from_terahashes(1.0)),
    );
    let steps: Vec<(&str, bm13xx::protocol::Register)> = vec![
        (
            "Core #1 (80 00 85 40)",
            bm13xx::protocol::Register::decode(
                bm13xx::protocol::RegisterAddress::Core,
                &[0x80, 0x00, 0x85, 0x40],
            ),
        ),
        (
            "Core #2 (80 00 80 20)",
            bm13xx::protocol::Register::decode(
                bm13xx::protocol::RegisterAddress::Core,
                &[0x80, 0x00, 0x80, 0x20],
            ),
        ),
        (
            "AnalogMux",
            bm13xx::protocol::Register::decode(
                bm13xx::protocol::RegisterAddress::AnalogMux,
                &[0x00, 0x00, 0x00, 0x03],
            ),
        ),
        (
            "IoDriverStrength",
            bm13xx::protocol::Register::decode(
                bm13xx::protocol::RegisterAddress::IoDriverStrength,
                &[0x02, 0x11, 0x41, 0x11],
            ),
        ),
        (
            "PllDivider (~50MHz, real captured value)",
            bm13xx::protocol::Register::decode(
                bm13xx::protocol::RegisterAddress::PllDivider,
                &[0x40, 0xa8, 0x02, 0x65],
            ),
        ),
        (
            "TicketMask",
            bm13xx::protocol::Register::TicketMask(TicketMask::new(asic_difficulty)),
        ),
        (
            "NonceRange (BM1370's fixed unpartitioned value)",
            bm13xx::protocol::Register::NonceRange(bm13xx::protocol::NonceRangeConfig::from_raw(
                0xB51E_0000,
            )),
        ),
    ];

    let mut previous_count = chips.len();
    for (name, register) in steps {
        println!("\n--- Step: {name} ---");
        data_writer
            .send(Command::WriteRegister {
                broadcast: true,
                chip_address: 0x00,
                register,
            })
            .await?;
        // Testing whether a longer settle alone (not more commands
        // flushing the bus) is what actually clears the post-Core
        // chatter burst found in the previous run.
        time::sleep(Duration::from_millis(1000)).await;

        data_writer.send(BM13xxProtocol::discover_chips()).await?;
        let (step_chips, step_bytes) =
            read_responses(&mut reader, Duration::from_millis(1000)).await?;
        report(chain, name, &step_chips, step_bytes);

        if step_chips.len() < previous_count {
            println!(
                "*** REGRESSION HERE: {name} dropped responsiveness from {previous_count} to {} chips. ***",
                step_chips.len()
            );
        }
        previous_count = step_chips.len();
    }

    println!(
        "\nFinal chip count after all steps: {previous_count} (started at {}).",
        chips.len()
    );

    println!("Disabling chain {chain} (GPIO {gpio} -> 0)...");
    let _ = fs::write(format!("/sys/class/gpio/gpio{gpio}/value"), "0");

    Ok(())
}

/// Read raw bytes directly (not through `FramedRead`) so we can tell
/// "chips are truly silent" apart from "chips are responding but
/// something about framing/CRC isn't matching" -- `FrameCodec`'s
/// decoder silently discards one byte at a time on any mismatch,
/// which would otherwise hide a protocol bug behind the same "0
/// responses" result as no electrical activity at all.
async fn read_responses(
    reader: &mut SerialReader,
    timeout: Duration,
) -> anyhow::Result<(Vec<u8>, usize)> {
    let mut chips = Vec::new();
    let mut total_bytes = 0usize;
    let mut buf = BytesMut::new();
    let mut codec = bm13xx::FrameCodec;
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        let mut chunk = [0u8; 256];
        tokio::select! {
            n = reader.read(&mut chunk) => {
                let n = n?;
                if n == 0 {
                    break;
                }
                total_bytes += n;
                println!("  raw rx ({n} bytes): {:02x?}", &chunk[..n]);
                buf.extend_from_slice(&chunk[..n]);

                while let Ok(Some(response)) = codec.decode(&mut buf) {
                    match response {
                        bm13xx::Response::ReadRegister {
                            chip_address,
                            register: bm13xx::Register::ChipId { chip_type, core_count, address },
                        } => {
                            let id = chip_type.id_bytes();
                            println!(
                                "  chip: resp_addr={chip_address:#04x} chip_id={:02x}{:02x} ({chip_type:?}) core_count={core_count:?} reg_address={address:#04x}",
                                id[0], id[1]
                            );
                            chips.push(address);
                        }
                        other => println!("  unexpected response: {other:?}"),
                    }
                }
            }
            _ = time::sleep_until(deadline) => break,
        }
    }

    Ok((chips, total_bytes))
}

fn report(chain: u8, label: &str, chips: &[u8], total_bytes: usize) {
    println!(
        "Chain {chain} ({label}): {} chip response(s), {total_bytes} raw byte(s) received.",
        chips.len()
    );
    if chips.len() == EXPECTED_CHIPS {
        println!("MATCHES expected {EXPECTED_CHIPS} chips/chain.");
    } else if total_bytes == 0 {
        println!("Zero raw bytes received -- no electrical/protocol activity at all on this UART.");
    } else {
        println!(
            "Received {total_bytes} raw byte(s) but only {} decoded chip(s) -- \
             framing/CRC/protocol mismatch, not silence. Check the raw rx hex above.",
            chips.len()
        );
    }
}
