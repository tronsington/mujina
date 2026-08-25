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
use mujina_miner::asic::bm13xx::{self, BM13xxProtocol, protocol::Command};
use mujina_miner::transport::serial::SerialStream;
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

    println!("Enabling chain {chain} via GPIO {gpio}...");
    sysfs_set_output_high(gpio)?;

    println!("Opening {tty} at {baud} baud...");
    let stream = SerialStream::new(&tty, baud)?;
    let (mut reader, writer, _control) = stream.split();
    let mut data_writer = FramedWrite::new(writer, bm13xx::FrameCodec);

    println!("Sending ChainInactive...");
    data_writer.send(Command::ChainInactive).await?;
    time::sleep(Duration::from_millis(10)).await;

    println!("Sending VersionMask (broadcast, x3)...");
    for _ in 1..=3 {
        let cmd = Command::WriteRegister {
            broadcast: true,
            chip_address: 0x00,
            register: bm13xx::protocol::Register::VersionMask(
                bm13xx::protocol::VersionMask::full_rolling(),
            ),
        };
        data_writer.send(cmd).await?;
        time::sleep(Duration::from_millis(5)).await;
    }
    time::sleep(Duration::from_millis(10)).await;

    println!("Sending discover_chips (broadcast ChipId read)...");
    let discover_cmd = BM13xxProtocol::discover_chips();
    data_writer.send(discover_cmd).await?;

    // Read raw bytes directly (not through FramedRead) so we can tell
    // "chips are truly silent" apart from "chips are responding but
    // something about framing/CRC isn't matching" -- FrameCodec's
    // decoder silently discards one byte at a time on any mismatch,
    // which would otherwise hide a protocol bug behind the same "0
    // responses" result as no electrical activity at all.
    let mut chips = Vec::new();
    let mut total_bytes = 0usize;
    let mut buf = BytesMut::new();
    let mut codec = bm13xx::FrameCodec;
    let timeout = Duration::from_millis(2000);
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

    println!(
        "Chain {chain}: {} chip response(s), {total_bytes} raw byte(s) received in {timeout:?}",
        chips.len()
    );
    if chips.len() == 77 {
        println!("MATCHES expected 77 chips/chain.");
    } else if total_bytes == 0 {
        println!("Zero raw bytes received -- no electrical/protocol activity at all on this UART.");
    } else {
        println!(
            "Received {total_bytes} raw byte(s) but only {} decoded chip(s) -- \
             framing/CRC/protocol mismatch, not silence. Check the raw rx hex above.",
            chips.len()
        );
    }

    println!("Disabling chain {chain} (GPIO {gpio} -> 0)...");
    let _ = fs::write(format!("/sys/class/gpio/gpio{gpio}/value"), "0");

    Ok(())
}
