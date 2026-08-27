# Mujina on the Antminer S19K Pro

Last updated 2026-08-25. This is the entry point for this effort —
read this first, then
[the hardware reference](../hardware.md) for
full hardware depth, and
[the recon methodology doc](recon-methodology.md) for the original
recon session's methodology (superseded on some specifics — see the
hardware notes' revision history — but still the accurate record of
what was actually done and why it was safe).

## The goal

Get [Mujina](https://github.com/256foundation/mujina) (open-source
Bitcoin mining firmware, Rust) actually driving the hashboards on a
real **Antminer S19K Pro**, currently running stock BraiinsOS+. This
is in support of a live community effort — see the
[256 Foundation forum thread][forum] where **Schnitzel**, **skot**,
and **AgentP** each have individual, unmerged forks doing pieces of
this same work. Nobody has a finished, working driver yet as of this
writing — this is genuine unsolved integration work, not "go copy an
existing driver."

[forum]: https://forum.256foundation.org/t/best-practices-for-hacking-mujina-onto-other-miners/48

## Status at a glance

| Piece | Status |
|---|---|
| Cross-compilation toolchain | **Working.** See [Toolchain & build recipe](#toolchain--build-recipe). |
| `mujina-minerd` running on real hardware | **Working.** Starts cleanly, exits cleanly. |
| PSU communication (protocol + transport) | **Solved.** Bit-banged GPIO I2C, full APW12 framing implemented and verified end-to-end against the real PSU. |
| Native `hw_trait::Gpio`/`I2c` implementations | **Working**, smoke-tested on real hardware. See [Native hw_trait implementations and the first working board driver](#native-hw_trait-implementations-and-the-first-working-board-driver). |
| `VirtualBoardDescriptor` daemon wiring | **Working**, smoke-tested on real hardware — mirrors `cpu_miner`'s existing pattern in `daemon.rs`/`backplane.rs`. |
| Real board driver (`src/board/antminer_s19k_am3.rs`) | **Mining real Bitcoin on real hardware; confirmed up to ~300MHz with _this project's own_ driver.** Chain presence/enable GPIO, 6 temperature sensors, and PSU voltage telemetry confirmed live over the real REST API. Creates one real `BM13xxThread` per chain; subscribes/authorizes/receives jobs from a real Stratum pool and dispatches correctly-framed jobs to real hardware. As of [Round 12](#round-12-solved--real-per-chip-values-extracted-from-the-existing-trace-and-mujina-mines-its-first-accepted-share), real `Response::Nonce` decodes stream, real shares get accepted, and (Round 14) 300MHz is confirmed solid (real rising temperature, ~4-6 TH/s measured hashrate). Every frequency above 300MHz failed identically here — flat temperature, zero nonces. **[Round 15](#round-15-solved--a-byte-order-bug-on-the-bm1366-core-register-was-disabling-almost-every-hashing-core) found the root cause** (a big/little-endian mix-up on the BM1366 `Core` register that left `CORE_MAILBOX`'s "all cores" bit clear, so core-enable never applied chain-wide) and reached **~85 TH/s at 575MHz with real accepted shares** — but that was verified against the **reference** binary. **This driver has not yet been retested with the fix**; doing so is the top next step. |
| Sustained ~100 TH/s-class hashing on this hardware | **Achieved and independently verified: 105.6 TH/s on the pool's own 5-minute API** — exceeding `bosminer`'s 104 TH/s on this same unit — with a **0% reject rate** at settled vardiff (166,909), best difficulty 39.9M, 49C at full fans. Via the Schnitzel reference binary plus a bit-bang PSU shim. See [Round 15](#round-15-solved--a-byte-order-bug-on-the-bm1366-core-register-was-disabling-almost-every-hashing-core). |
| BM13xx chip discovery over native UART | **Solved and wired into the real driver.** `s19k-probe` (standalone tool) and `board/antminer_s19k_am3.rs` (the real driver, as of Round 6) both get clean 77/77 chip responses on real hardware. See [Round 5](#round-5-chip-discovery-actually-works-77-77-chip-responses-on-real-hardware) and [Round 6](#round-6-discovery-wired-into-the-real-board-driver). |
| BM1366 register-init/mining sequence | **Solved.** Full VersionMask/InitControl/MiscControl/ChainInactive/SetChipAddress/per-chip-config/discover sequence confirmed working (Round 5), and as of Round 12 the per-chip pass uses this board's real captured values — real jobs dispatch, chips respond with real nonces, and the pool accepts shares. The chips self-report as **BM1366**, not BM1362 — see [Round 5](#round-5-chip-discovery-actually-works-77-77-chip-responses-on-real-hardware) through [Round 12](#round-12-solved--real-per-chip-values-extracted-from-the-existing-trace-and-mujina-mines-its-first-accepted-share). |

## Target hardware and access

- Miner: `ssh root@<miner-ip>` — no password, already open (this is
  the BraiinsOS+ default, not an exploit).
- `bosminer`/`boser` are the stock daemons currently running and
  hashing normally.
- Stop/start them via `/etc/init.d/S99bosminer {stop,start}` — this
  is a graceful stop (the script waits for the process to actually
  exit) that has been used extensively and safely throughout this
  work. **Never `kill -9` them.**
- Working dev container: `<dev-host-ip>`, user `exergy`. Has gcc/git/
  curl; no passwordless sudo (ask the owner for anything needing
  `apt-get`). Can SSH directly to the miner.

## Risk posture

The miner's owner has explicitly and repeatedly accepted this unit as
a sunk cost — bricking it during this work is an accepted risk, not
something to over-guard against. Routine risk (stopping mining to
test something, restarting it, GPIO reads/writes, running new
binaries) doesn't need permission. Stage things sensibly anyway —
one change at a time, prefer graceful/known-good paths (watching
`bosminer`'s own behavior) over blind guessing, since that approach
has consistently produced better signal than guessing throughout this
project. Flag explicitly (don't just proceed silently) before doing
something in a genuinely different risk category — writing to flash,
a full firmware reflash, editing the devicetree and rebooting, or
anything else that could cause a longer or less-recoverable outage
than "mining pauses for a few minutes."

**Hard exception, not covered by the sunk-cost tolerance above: never
disable or risk disabling the cooling fans**, for any reason, at any
point. The owner's words: "make sure to never turn the fans off. I
don't want to burn this thing up or burn the building down." This is
a fire/property-safety line, not a hardware-risk-tolerance one — it
doesn't loosen just because the miner itself is expendable. Concretely:
never write to `/sys/class/pwm/pwmchip0/{pwm0,pwm1}/enable` to disable
them or drop `duty_cycle` low, and after any experiment that touches
PSU power or chain-enable state, verify fans are still spinning at a
healthy RPM via `bosminer`'s live metrics — `grep fan_rpm_feedback
/etc/log/metrics/metrics.prom | tail -5` (expect ~5000+ RPM per
populated fan) and confirm `miner_pause_condition{reason="broken
fans"}` reads `0`. GPIO tach-line polling
(`/sys/class/gpio/gpio447-450/value`) is **not** reliable for this —
fan pulses are far too fast for sysfs polling to resolve reliably;
the metrics file is the trustworthy source.

**One incident already happened under this policy and is fully
documented, not swept under the rug** — see
[EEPROM corruption incident](#eeprom-corruption-incident-and-repair)
below. It's a good illustration of where the line actually sits:
routine GPIO/I2C experimentation is fine, but *any* third-party tool
run against a live address needs its actual read/write behavior
verified (ideally by reading its source), not assumed from its name
or docs.

## Toolchain & build recipe

**The target is `armv7-unknown-linux-musleabihf`, not any `aarch64`
target.** See
[the hardware notes' architecture section](../hardware.md#architecture--read-this-before-assuming-anything-about-the-target-triple)
for why — short version: the kernel is 64-bit but the actual
userspace is 32-bit ARM hard-float, confirmed by `readelf` on the
real `bosminer` binary.

In a fresh container (confirmed working from a clean `exergy` home
directory, no root):

1. **Bootstrap Rust**: `curl --proto '=https' --tlsv1.2 -sSf
   https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
   --profile minimal`, then `rustup target add
   armv7-unknown-linux-musleabihf`.

2. **Get zig** as the cross C compiler/linker (no `apt-get install`
   needed, no sudo, no musl-cross-toolchain package required):
   fetch `https://ziglang.org/download/index.json`, pick the current
   stable version, download `.../zig-x86_64-linux-<ver>.tar.xz`,
   extract anywhere (`~/tools` was used here; 0.16.0 at time of
   writing).

3. **Two wrapper scripts are required** — invoking `zig cc -target
   arm-linux-musleabihf` directly is not enough, for two subtle
   reasons cargo's build system creates:

   `~/bin/zigcc-armv7-musleabihf`:
   ```sh
   #!/bin/sh
   args=""
   for a in "$@"; do
     case "$a" in
       --target=*) continue ;;
     esac
     args="$args \"$a\""
   done
   eval exec "/path/to/zig" cc --target=arm-linux-musleabihf $args
   ```
   - `cc-rs` (used by build scripts that compile C code, e.g. `ring`)
     passes `--target=armv7-unknown-linux-musleabihf` — the *Rust*
     target triple — directly to the C compiler. zig's own target
     parser doesn't recognize `armv7` as an architecture name (it
     wants `arm`), so that flag must be stripped and replaced, not
     just appended to.
   - rustc's **final link** invocation of the external linker does
     **not** pass `--target=` at all (unlike a compile step) — it
     assumes the "linker" program is already configured for the
     right target, the way a real cross-`gcc` would be. Without the
     wrapper unconditionally forcing `--target=arm-linux-musleabihf`
     on every invocation, the link step silently falls back to zig's
     host default (x86_64) and produces a binary that fails to link
     against the real arm32 `.rlib`s.

   `~/bin/zigar-armv7-musleabihf`:
   ```sh
   #!/bin/sh
   exec /path/to/zig ar "$@"
   ```

4. **Build environment**:
   ```sh
   export CC_armv7_unknown_linux_musleabihf=zigcc-armv7-musleabihf
   export AR_armv7_unknown_linux_musleabihf=zigar-armv7-musleabihf
   export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_MUSLEABIHF_LINKER=zigcc-armv7-musleabihf
   export RUSTFLAGS="-C link-self-contained=no"
   ```
   The `RUSTFLAGS` line is required: without it, rustc links in its
   own bundled self-contained musl CRT objects (`crt1.o` etc.) *and*
   zig links in its own bundled musl CRT objects, producing `ld.lld:
   error: duplicate symbol: _start`. `link-self-contained=no` defers
   all CRT objects to zig's bundled musl libc instead, which is the
   one that actually matches the linker being used.

5. **Apply the musl-compat patch** (below) to the `mujina` source
   tree, then:
   ```sh
   cargo build --release --target armv7-unknown-linux-musleabihf --bin mujina-minerd
   ```

6. **Deploy**: the miner's BusyBox has no `sftp-server`, so plain
   `scp` fails ("Connection closed"). Force the legacy protocol:
   `scp -O <binary> root@<miner-ip>:/tmp/<binary>`.

Result: a small (~18MB unstripped release), fully static ELF32 ARM
binary — confirmed running cleanly on the real miner (`mujina-minerd`
starts its daemon, dummy job source, and API server on
`127.0.0.1:7785`, and exits cleanly on `SIGTERM`).

### The musl-compat patch

Mujina doesn't compile cleanly for a musl target out of the box:
`reqwest` pulls in a TLS backend that's painful under musl, and
`udev`/`tracing-journald` are glibc-only. This mirrors the shape of
upstream's open, unmerged
[PR #55](https://github.com/256foundation/mujina/pull/55) (contested
by the maintainer on *design* grounds — they want opt-in Cargo
features instead of libc-gating — but the underlying patch works).
Four edits, all gating on `target_env` rather than bare
`target_os = "linux"`:

```diff
# Cargo.toml (workspace root)
-reqwest = { version = "0.12", features = ["json"] }
+reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls", "charset", "http2"] }

# mujina-miner/Cargo.toml
-[target.'cfg(target_os = "linux")'.dependencies]
+[target.'cfg(all(target_os = "linux", target_env = "gnu"))'.dependencies]
 tracing-journald = { workspace = true }
 udev = { workspace = true }

# mujina-miner/src/tracing.rs — both occurrences:
-#[cfg(target_os = "linux")]
+#[cfg(all(target_os = "linux", target_env = "gnu"))]
 mod journald { ... }
-#[cfg(not(target_os = "linux"))]
+#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
 mod journald { ... }

# mujina-miner/src/transport/usb.rs — the linux/platform module selection:
-#[cfg(target_os = "linux")]
+#[cfg(all(target_os = "linux", target_env = "gnu"))]
 mod linux;
-#[cfg(target_os = "linux")]
+#[cfg(all(target_os = "linux", target_env = "gnu"))]
 use linux as platform;
 ...
-#[cfg(not(any(target_os = "linux", target_os = "macos")))]
+#[cfg(not(any(all(target_os = "linux", target_env = "gnu"), target_os = "macos")))]
 mod platform { ... }
```

This patch is already applied in the working local clone at
`~/mujina` on the `<dev-host-ip>` container (commit
`dfc6b0d`, "Add armv7-musleabihf musl-compat patch and S19K Pro probe
tool").

## Mujina's actual architecture

(From reading the real source — `mujina-miner/src/board/README.md` is
stale and describes a `Board` trait that doesn't exist. Don't trust
it.)

- **Board registration**: boards register via `inventory::submit! {
  BoardDescriptor { pattern, name, create_fn } }` in
  `src/board/mod.rs`. `BoardPattern` matches USB VID/PID/manufacturer
  /product/serial. There's also a **`VirtualBoardDescriptor`** path
  (`device_type`, `name`, `create_fn: fn() -> BoxFuture<Result
  <BackplaneConnector>>`, no device param) — **this is what our board
  should use**, since it isn't a USB peripheral at all; it *is* the
  host system. Neither existing driver (`bitaxe.rs`, `emberone00.rs`)
  uses this path — both are USB boards — so this is new ground for
  the project.

- **`BackplaneConnector`** (returned by the factory fn): `{ info:
  BoardInfo, threads: Vec<Box<dyn HashThread>>, telemetry_rx:
  watch::Receiver<BoardTelemetry>, shutdown: Option<BoxFuture<...>>
  }`.

- **The trait that actually matters**: `HashThread` in
  `src/asic/hash_thread.rs` — `configure()`, `update_task()`,
  `replace_task()`, `go_idle()`, `status()`, etc. Boards don't
  implement this directly — they build a `BoardPeripherals {
  asic_enable: Option<Box<dyn AsicEnable>>, voltage_regulator:
  Option<Box<dyn VoltageRegulator>> }` and hand it to
  `BM13xxThread::new(name, chip_responses, chip_commands,
  peripherals, removal_rx)` (`src/asic/bm13xx/thread.rs`), which does
  the real work and returns something implementing `HashThread`.

- **The BM13xx chip protocol is genuinely reusable**:
  `src/asic/bm13xx/` (`mod.rs`, `protocol.rs`, `crc.rs`, `error.rs`,
  `thread.rs`) is chip-family-generic (BM1362/1366/1368/1370 via a
  `ChipType` enum), independent of any specific board. Full protocol
  spec with exact byte sequences lives at
  `mujina-miner/src/asic/bm13xx/REFERENCE.md` (not under `docs/`, as
  the README implies) — frame format, CRC5 (commands, poly `0x05`/
  init `0x1F`) and CRC16 (jobs, CRC-16-CCITT-FALSE), and literal
  example frames.

- **`transport::serial::SerialStream::new(path: &str, baud_rate:
  u32)` already exists** and is generic/non-USB-tied — exactly what's
  needed to open `/dev/ttyS1`/`ttyS2`/`ttyS3` directly. No new
  transport code needed for basic UART access.

- **The GPIO/I2C gap is now filled.** Both `bitaxe.rs` and
  `emberone00.rs` get GPIO/I2C by tunneling a custom "bitaxe-raw"
  packet protocol over a `tokio-serial` UART connection **to an
  RP2040 co-processor** running known firmware. **Our board has no
  such co-processor** — it exposes GPIO/I2C/UART directly from the
  Amlogic SoC to the host Linux kernel. No crate existed for this and
  no `hw_trait` implementation used it; `src/linux_hw/` now provides
  one, hand-rolled (no `gpiocdev`/`i2cdev`/etc. dependency added) —
  see
  [Native hw_trait implementations and the first working board driver](#native-hw_trait-implementations-and-the-first-working-board-driver)
  below.

- **Virtual-board dispatch is now generic (well, generalized once,
  for our board).** `VirtualBoardDescriptor` + `inventory::submit!`
  existed and worked, but `backplane.rs` used to only wire up
  `"cpu_miner"` specifically. The same additive plumbing pattern
  `cpu_miner` used (a `TransportEvent` variant, a `daemon.rs`
  env-gated startup block, a `backplane.rs` handler) has now been
  added for `antminer_s19k_am3` too, confirmed working end-to-end on
  real hardware.

- **BM1362 register-init values don't exist anywhere in the
  community yet.** `emberone00.rs` (same chip!) never finished its
  implementation — it returns `threads: Vec::new()` with a
  `warn!("emberOne/00 hash threads not yet implemented")`. `bitaxe.rs`
  has a full working init sequence but for **BM1370**, with
  chip-specific magic register constants (e.g. `Core { raw_value:
  0x8000_8B00 }`) that don't transfer directly. skot's diagnostic
  tool doesn't do a full init either. **Deriving correct BM1362 init
  values (empirically, against the real hardware) is real, novel
  work** — `bitaxe.rs`'s sequence shape (VersionMask → ChainInactive
  → SetChipAddress loop → Core/InitControl/MiscControl writes →
  TicketMask → PLL ramp 56.25→525MHz in 6.25MHz steps → NonceRange)
  is the structural template to adapt, not values to copy.

- **`Fan`/`TemperatureSensor`/`PowerMeasurement` telemetry structs
  already exist** in `src/api_client/types.rs` (`BoardTelemetry`) —
  just need feeding.

## What's built and verified

Both probes below live in the `mujina` clone (a fresh fork of
`256foundation/mujina`, not skot's or rkuester's), as new `[[bin]]`
targets in `mujina-miner/Cargo.toml`, and build with the toolchain
above.

### `mujina-minerd` itself

Runs cleanly on the real hardware. `MUJINA_LOG=info /tmp/mujina-minerd`
produces:

```
INFO daemon: Using dummy job source (set MUJINA_POOL_URL to use Stratum v1)
INFO daemon: Started.
INFO api::server: API server listening.
               url=http://127.0.0.1:7785
```

and exits cleanly on `SIGTERM`. No board driver exists yet, so there's
nothing more interesting to observe here — this just proves the
toolchain and binary are sound on the real target.

### `s19k_probe.rs` — chip discovery over native UART

A minimal standalone tool reusing Mujina's real
`bm13xx::FrameCodec`/`BM13xxProtocol` and
`transport::serial::SerialStream` to enable one hashboard chain via
sysfs GPIO and attempt `discover_chips()` over the real UART,
independent of the full backplane/board-driver machinery.

```
s19k-probe <chain 1|2|3> [baud] [tty-override]   # baud defaults to 115200
```

**Result: 0 chip responses**, including after the PSU bit-bang
breakthrough with real, confirmed PSU power applied — see
[Chip discovery: still silent, several hypotheses now ruled out](#chip-discovery-still-silent-several-hypotheses-now-ruled-out)
for the full, more recent investigation. Also gained a `tty-override`
argument (to cross-match which GPIO enables against which UART
independently) and raw-byte-level receive logging (to distinguish
true silence from a decode bug) since the description below was
written.

### `psu_bitbang_probe.rs` — PSU communication, fully solved

This is the significant result of this session. Standard kernel I2C
tracing (`/dev/i2c-0`, `/dev/i2c-1`, and 5 minutes of every relevant
`i2c`/`smbus` ftrace tracepoint across a full cold boot) never showed
any PSU traffic, because the real PSU bus is a **software bit-banged
I2C bus on GPIO 476 (SCL) / 477 (SDA)** — confirmed straight from
Bitmain's own `/etc/init.d/S37board_setup` boot script, which labels
these two lines `I2C_SCL`/`I2C_SDA` in plain comments. Bit-banged GPIO
writes never touch the kernel's `i2c_transfer()`, so no amount of
kernel-level I2C tracing was ever going to catch this — the earlier
search for it in the wrong place wasn't a matter of insufficient
capture time.

`psu_bitbang_probe.rs` implements, from scratch:

1. A software I2C master over sysfs GPIO 476/477 — `START`/`STOP`,
   bit-level clock/data with open-drain emulation via direction
   switching (`in` = release/pulled-high, `out`+`0` = drive low),
   byte-level write-with-ACK-check and read-with-controlled-ACK/NAK.
2. The APW12 frame protocol on top — preamble, length, command,
   payload, 16-bit checksum, ported from
   `amlogic-cb-tools/src/protocol.rs`'s `build_frame`/`parse_frame`/
   `checksum` (a standalone Rust diagnostic toolkit by community
   member skot, not part of Mujina) — and the request/response
   `exchange()` flow (send one byte at a time to register `0x11`,
   poll for a response frame with NAK/retry handling), matching the
   shape of `apw12-psu-tool`'s reference implementation but running
   over the real working transport instead of its non-functional-
   on-this-board `/dev/i2c-1` path.

Full protocol detail (frame format, transaction mechanics, command
bytes) is in
[the hardware notes' PSU section](../hardware.md#psu-communication--this-is-not-on-either-hardware-i2c-bus).

**Verified against the real PSU** (`bosminer` stopped first — it
bit-bangs these same two lines itself, and running two software
masters on the same physical wires concurrently is exactly what
caused the incident below):

```
$ psu-bitbang-probe get-fw
firmware payload: [19, 00]
raw: [55, AA, 06, 01, 19, 00, 20, 00]

$ psu-bitbang-probe get-hw
hardware payload: [75, 00]
raw: [55, AA, 06, 02, 75, 00, 7D, 00]

$ psu-bitbang-probe read-state
state=0x0000 (OFF)
raw: [55, AA, 06, 05, 00, 00, 0B, 00]

$ psu-bitbang-probe get-voltage
dac_code=0xE2 (226)
estimated_voltage=12.1600 V
raw: [55, AA, 06, 03, E2, 00, EB, 00]

$ psu-bitbang-probe measure-voltage
adc_raw=0x0001 measured_voltage=0.0295 V
raw: [55, AA, 06, 04, 01, 00, 0B, 00]
```

The firmware version (`0x0019`) and hardware version (`0x0075`)
**match `bosminer`'s own log output for this exact PSU exactly**
(`PSU: FW version 0x0019`, `PSU: Version '0x75' (APW121215d/e)`) —
independent cross-validation that the whole reimplementation (bit-
bang transport plus frame protocol) is correct, not just plausible-
looking. `read-state`/`measure-voltage` correctly report the PSU as
off, since it wasn't enabled during the test. Every response passed
checksum validation (a mismatched checksum surfaces as a parse error,
not a silently-accepted garbage value).

Read-only commands (`get-fw`/`get-hw`/`get-voltage`/`measure-voltage`
/`read-state`/`disable-watchdog`) and `output-on`/`output-off` (PSU_nEN
/ GPIO 437) are ready to use any time (stop `bosminer` first).
`set-voltage <volts>` is also wired up (verified by an immediate
readback rather than trusting the write landed) — see
[Round 3 of the chip-discovery investigation](#round-3-found-the-real-voltage-target-152v-tested-at-it-directly-still-silent)
for how it was used to test discovery at `bosminer`'s own real
target voltage. `set-dac`/`enable-watchdog` remain implemented in the
protocol layer but not wired into the CLI — no need for them yet.

## Chip discovery: solved, after five rounds of investigation

The obvious next experiment after solving PSU communication was
retrying `s19k-probe` with the PSU actually and correctly enabled.
That took five rounds — four of silence and dead ends, documented in
full below because a clean series of ruled-out hypotheses is exactly
what saves the next person (or the next session) from repeating this
work — before [Round 5](#round-5-chip-discovery-actually-works-77-77-chip-responses-on-real-hardware)
actually got clean 77/77 chip responses on real hardware.

### Round 1: real PSU power, GPIO state, numbering, timing

**Confirmed real PSU power was present** before every test below:
`psu-bitbang-probe output-on` (disabling the comms watchdog first),
then `measure-voltage` confirming ~13.1-13.2V actually present on the
rail (not just a GPIO flipped with no way to know if it did
anything) — this is genuine DC power reaching the hashboards, not an
assumption.

**Added raw-byte-level diagnostics to `s19k_probe.rs`** specifically
to answer one question precisely: are the chips truly electrically
silent, or are they responding but failing to decode (which would
point at a protocol/CRC bug in our own code instead of a hardware
gap)? The tool now reads raw bytes directly off the UART (bypassing
`FrameCodec`, which silently discards one byte at a time on any
mismatch — exactly the kind of behavior that would hide a decode bug
behind an identical-looking "0 responses" result) and reports total
raw bytes received alongside decoded chip count.

**Result: 0 raw bytes, every single time**, across every combination
tried:

- Chain 1 alone enabled (GPIO 454) vs. all three chains enabled
  together (matching `bosminer`'s own known-good steady-state GPIO
  values exactly).
- Both 115200 and 3,125,000 baud.
- **All three UART devices cross-matched against GPIO 454 alone**
  (`ttyS1`, `ttyS2`, `ttyS3`, via a new `tty-override` probe
  argument) — ruling out a chain-to-tty numbering mix-up as the
  explanation. (This was a real concern: `S37board_setup`-adjacent
  sources number hashboards HB0/HB1/HB2 → `ttyS3`/`ttyS2`/`ttyS1`,
  which conflicts with the naive chain-N→`ttyS`N mapping the probe
  code uses and this document's UART table currently states. Neither
  mapping produced a response in this test, so the question is
  unresolved but no longer blocking — see the open item below.)
- **A proper cold power-on-reset sequence**: all three chain-enable
  lines driven low (reset asserted) *before* enabling PSU output,
  PSU enabled and voltage confirmed stable, *then* reset released —
  in case chips latch state during the power ramp and a reset
  toggled after power was already sitting stable for several minutes
  doesn't count as a real power-on event. No change.

**Isolating check**: immediately after all of the above (same PSU/
GPIO hardware state), restarted `bosminer` itself. It discovered
77/77 chips on all three chains within about 2 seconds, as normal.
**This confirms the hardware is completely fine** — every chip on
every chain responds instantly to the real firmware's own sequence.
The gap is specifically in what our own code does differently from
`bosminer`'s (closed-source) bring-up, not a hardware fault, wiring
issue, or anything discovered/changed during this session's
experimentation.

**Also checked and ruled out**: UART line settings. `stty -F
/dev/ttyS1 -a` while `bosminer` held the port open and mining
successfully shows plain 8N1, no parity, no flow control (`cs8
-cstopb -parenb -crtscts -ixon -ixoff`) — completely standard, and
almost certainly what `tokio-serial`/`SerialStream` already sets by
default. Not a termios mismatch.

### Round 2: fixed a real protocol-sequence bug, verified wire bytes exactly, still silent

The probe's command sequence had a real bug, found by re-reading
Mujina's own BM13xx protocol reference more carefully. It was
sending `ChainInactive` → three `VersionMask` writes → discovery —
copying the shape of `bitaxe.rs`'s BM1370/S21-Pro pattern. But
`mujina-miner/src/asic/bm13xx/REFERENCE.md`'s "Multi-Chip
Initialization" section documents the **actual captured** sequence
for a **S19j Pro** chain (BM1362, the same chip family and closest
known relative to our hardware): **one** `VersionMask` write, *then*
discovery, with `ChainInactive` coming later, only as part of address
assignment — not as a discovery prerequisite at all. (Its own
[^chaininit] footnote spells this out explicitly: "The S19j Pro sends
one write before discovery and two after.") `ChainInactive`'s
documented purpose is specifically enabling the interception behavior
`SetChipAddress` needs, not gating ordinary broadcast reads.

Fixed `s19k_probe.rs` to send the real captured sequence
(`VersionMask` ×1 → discovery, no `ChainInactive` first). **Still
zero responses** — but this was a real, worth-fixing bug regardless
of outcome, and rules out "wrong command sequence" as an explanation
going forward.

**Verified the actual wire bytes precisely**, via the crate's
built-in trace logging (`MUJINA_LOG=trace`), rather than trusting the
code to be doing what it's supposed to:

```
VersionMask: 55 aa 51 09 00 a4 90 00 ff ff 1c
discover:    55 aa 52 05 00 00 0a
```

Both match `REFERENCE.md`'s documented, verified example frames
**byte-for-byte** (`55 AA 51 09 00 A4 90 00 FF FF 1C` and `55 AA 52
05 00 00 0A` respectively). This conclusively rules out a
frame-encoding bug — the bytes actually leaving the UART are exactly
correct, textbook BM13xx protocol.

**Tried settle delays** of up to 3 seconds between chain-enable and
the first UART command (in case chips need real time after power/
reset before they're ready — `bosminer`'s own ~1.9-second gap between
"Initializing hashchain" and the first discovery response looks far
too long to be pure communication time for a sub-millisecond
broadcast/response exchange, suggesting a deliberate delay is part of
its real sequence). No change at any delay tried.

### Round 3: found the real voltage target (~15.2V), tested at it directly, still silent

Tried `RUST_LOG=trace` before restarting `bosminer`, on the theory
that its module names (`bosminer_am2_s17`, `bosminer_backend`, etc.)
look like standard Rust `tracing`-crate targets and it might respect
the same near-universal env var Mujina itself does. **It didn't** —
no `TRACE`/`DEBUG` lines appeared, only the usual `INFO`/`WARN`. (One
operational note for next time: `bosminer`'s log lives on the
flash-backed `/etc` partition, ~15MB free, not tmpfs — keep any
verbose-logging experiment brief.)

The attempt wasn't wasted, though — the resulting log capture
revealed something much more valuable by accident. Two independent
restarts showed a **consistent, deliberate ~13-17 second sequence**
between `bosminer` starting and actually initializing the hashchains
(PSU firmware/serial queries with multi-second gaps between each),
which is far longer than the ~2-second gap between "Initializing
hashchain" and "Discovered" that earlier testing had been measuring
and trying to replicate as a settle delay. A third restart, made
while chips were still warm from prior activity, showed *why*
directly: `bosminer` waits out a thermal cooldown, then runs an
explicit, gradual **PSU voltage ramp**, logged in full:

```
PSU: Ramping voltage 12.722 V -> 15.200 V (slow)
PSU: Setting voltage 12.722 V -> 13.222 V
PSU: Setting voltage 13.222 V -> 13.722 V
PSU: Setting voltage 13.722 V -> 14.222 V
PSU: Setting voltage 14.222 V -> 14.565 V
PSU: Setting voltage 14.565 V -> 14.787 V
PSU: Setting voltage 14.787 V -> 14.932 V
PSU: Setting voltage 14.932 V -> 15.025 V
PSU: Setting voltage 15.025 V -> 15.087 V
PSU: Setting voltage 15.087 V -> 15.126 V
PSU: Setting voltage 15.126 V -> 15.200 V
```

`Initializing hashchain` (and successful discovery ~2s later) follows
immediately after this ramp completes. **The real target output
voltage is ~15.2V** — every prior round of chip-discovery testing had
only ever used whatever the PSU's last-persisted setpoint happened to
be, generally in the ~12-13V range. This looked like an extremely
strong, well-evidenced lead.

Added a `set-voltage` command to `psu-bitbang-probe` (wiring up the
previously-implemented-but-unexposed `SET_VOLTAGE` protocol command
and porting `encode_voltage_to_dac` from skot's reference tool),
verified by an immediate `GET_VOLTAGE` readback rather than trusting
the write to have landed correctly.

**Tested chip discovery at confirmed real voltage — still zero
responses.** In fact the PSU turned out to already be sitting at
`dac=0x00` → 15.1084V nominal (the maximum this DAC formula can
express; `bosminer`'s exact 15.200V is ~90mV beyond it, likely a
small per-unit calibration difference this ported generic formula
doesn't capture) from `bosminer`'s own last successful run — its
setpoint persists across a graceful stop, `output-on` just
re-enables output at whatever was last configured. `measure-voltage`
independently confirmed ~15.15-15.17V genuinely present on the rail.
Retested chain 1 alone, all three chains together, and all three
UARTs, all at this confirmed real voltage — **zero raw bytes, every
time**, matching every earlier round exactly.

One loose end worth a note rather than a conclusion: voltage readings
bounced unexpectedly between two `measure-voltage` calls a few
seconds apart (12.2V, then 15.15V) with nothing between them but a
GPIO toggle and a failed discovery attempt. Not chased further — could
be genuine unloaded-PSU regulation behavior (no chips are drawing
current, since none are responding), or noise in the bit-banged ADC
read (a single bad bit swings the 16-bit raw value substantially).
Worth keeping in mind if anyone picks this up again.

Confirmed no regression from any of this: immediately after, restarted
`bosminer` and it discovered 77/77 chips on all three chains normally,
fans stayed healthy throughout (verified via the metrics file per the
[fan safety rule](#risk-posture)), and disk usage on the flash-backed
log partition barely moved.

### Round 4: real wire capture via a ptrace tracer — the assumed command order was backwards

No hardware logic analyzer was available, so instead of sniffing the
wire electrically, `bosminer` itself was traced at the syscall level:
a custom `s19k-trace` tool (`src/bin/s19k_trace.rs`, see
[below](#s19k_tracers---a-custom-ptrace-syscall-tracer)) launches
`bosminer` under `PTRACE_TRACEME`, decodes every `open`/`openat`/
`write` syscall across all its threads, and resolves each `write`'s
fd back to a real path (`/dev/ttyS3`, `/dev/i2c-1`, etc.) via
`/proc/<pid>/fd`. Run against a real, successful bring-up
(`/usr/bin/bosminer --log-to-file`, launched directly in place of the
supervised instance, then the supervised instance restored
afterward), this captured the complete real command sequence for the
first time — no guessing, no reimplementation, the actual bytes
`bosminer` sends before every chip responds.

**The result rewrites the model this whole investigation had been
working from.** `REFERENCE.md`'s "one `VersionMask` write, then
discovery" sequence is real but incomplete — it's only the *first*
few frames of a much longer bring-up. The full real sequence per
chain, condensed (see `/tmp/trace.log` on the miner while a trace is
running, or rerun `s19k-trace` to reproduce):

```
55 aa 51 09 00 a4 90 00 ff ff 1c   (VersionMask broadcast — sent 3x, byte-identical)
55 aa 51 09 00 a8 00 07 00 00 03   (broadcast write, different register)
55 aa 51 09 00 18 ff 0f c1 00 00   (broadcast write, different register again)
55 aa 53 05 00 00 03               (broadcast — sent 3x, byte-identical)
55 aa 40 05 00 00 1c               (SetChipAddress: addr 0x00)
55 aa 40 05 02 00 01               (SetChipAddress: addr 0x02)
   ... one 40 05 frame per chip, addr incrementing by 2, up to 0x9a
       (77 chips x 2 — matches "expected 77 chips" exactly) ...
55 aa 41 09 8a 58 02 11 41 11 0f   (per-chip addressed write, addr=0x8a)
55 aa 41 09 7c 58 02 11 41 11 1a   (per-chip addressed write, addr=0x7c)
   ... one 41 09 frame per already-addressed chip (PLL/config
       registers 0x58 and 0x2c seen), NOT in address order ...
55 aa 52 05 00 00 0a               (discover — THE VERY LAST STEP, not the first)
55 aa 51 09 00 70 00 00 00 00 18   (more broadcast writes follow, post-discovery)
```

**The critical correction: `52 05` ("discover") is sent *after* every
chip has already been individually addressed (`40 05` sweep) and
individually configured (`41 09` per-chip writes), not before.**
Every prior round tried `VersionMask → discover` in isolation,
skipping the address-assignment and per-chip PLL-config phases
entirely — which would explain total silence if (as is typical for
this chip family) BM1362s are UART daisy-chained relay devices that
don't pass bytes through an un-addressed chain at all, or don't
respond to a bare broadcast poll before being individually
configured. This isn't a parameter or timing bug like every earlier
round chased — the whole *shape* of the assumed protocol was
inverted. It also finally supplies real, captured bytes for the "BM1362
register-init sequence" line in the status table above, previously
marked not started.

**Not yet done**: this round captured real *sent* bytes only — the
tracer decodes `write()` but not `read()` payload content (see the
tracer's known limitations below), so the chips' actual responses
during this sequence weren't captured, only inferred from
`bosminer`'s own log line ("Discovered 77 chips") after the fact.
Reimplementing this exact write sequence in `s19k_probe.rs` (address
sweep, then per-chip config, then discover) and adding real `read()`
decoding to the tracer are the two clear next steps — see
[Next steps](#next-steps).

### `s19k-trace` — a custom ptrace syscall tracer

Built as a software alternative to a hardware logic analyzer (none
was available). `src/bin/s19k_trace.rs`, registered as `s19k-trace`
in `Cargo.toml`:

```
s19k-trace <program> [args...]
```

Launches `<program>` under `PTRACE_TRACEME` and logs every
`open`/`openat`/`write` syscall to stdout with a monotonic timestamp,
resolving `write` targets to real filesystem paths (not raw fd
numbers) via `/proc/<pid>/fd`. Implementation notes, since none of
this is obvious from the ptrace man page alone:

- Uses `libc::syscall(libc::SYS_ptrace, ...)` rather than
  `libc::ptrace()` directly — the latter is C-variadic, which is
  fragile to call correctly from Rust FFI (easy to pass the wrong
  argument types/count with no compiler check).
- **Must trace every thread, not just the main one.** The first
  version only followed the main pid and appeared to work (no
  crashes, plausible-looking output) but silently missed all of
  `bosminer`'s actual hardware I/O, which happens on separate
  `Serial/R/N`/`Serial/W/N` worker threads spawned per chain. Fixed
  via `PTRACE_O_TRACECLONE|TRACEFORK|TRACEVFORK` plus `waitpid(-1,
  ...)` to catch every thread/child, with per-tid state tracked in a
  `HashMap`. This bug is exactly why the ttyS opens weren't visible in
  an early single-threaded test run and were initially (wrongly)
  read as "bosminer never opens a UART at all."
- **fd-to-path resolution must happen on `open`'s *exit*, not
  entry** — refreshing the fd map when `open`/`openat` is *called*
  races against the fd not existing yet, and under heavy concurrent
  fd churn from multiple threads this produced wrong-path attribution
  (a GPIO direction write was once mislabeled as `/dev/i2c-1`). Fixed
  by tracking pending opens per-tid and refreshing only at the
  matching syscall-exit stop.
- Known limitation, not yet fixed: only `open`/`openat`/`write` are
  decoded with real content; every other syscall (including `read`,
  so chip *responses* aren't visible, only what `bosminer` sends) is
  tallied by number/count/last-args in a periodic summary rather than
  logged individually, to avoid flooding the flash-backed log
  partition during long traces.
- Overhead is real: full syscall-stop tracing (two context switches
  per syscall) slows a normal ~35-second bring-up to roughly
  190-200 seconds. Not a problem for capture purposes, but don't
  expect trace timestamps to line up with `bosminer`'s own untraced
  log timestamps.
- Usage: stop the supervised instance first (`/etc/init.d/S99bosminer
  stop`), run `s19k-trace /usr/bin/bosminer --log-to-file` directly
  (skip the `bos-tools run-and-watch` supervisor — it's just an
  OOM-score wrapper, irrelevant to tracing), then when done, `kill
  -KILL` both the tracer and the (likely ptrace-stopped, `SIGTERM`-
  immune) traced child before restarting the supervised instance —
  killing only the tracer leaves `bosminer` orphaned in a `T`
  (stopped) state that ignores `SIGTERM`.

**What's not yet been tried or explained**:

- The HB0/HB1/HB2 ↔ `ttyS3`/`ttyS2`/`ttyS1` numbering conflict noted
  above is still technically open — the trace confirms these are the
  three real device paths in active use (no other tty or memory-mapped
  device is ever touched), but didn't establish which physical chain
  each corresponds to (thread scheduling order isn't a reliable proxy
  for chain number). Low priority now: once discovery actually works
  using the corrected sequence above, a wrong chain/tty pairing will
  be immediately obvious (that chain alone stays silent while the
  other two succeed).
- `bosminer` doesn't respond to `RUST_LOG` — its actual log-level
  control mechanism (a different env var? a config file? hardcoded?)
  is unknown. Less urgent now that the ptrace tracer gives direct
  ground-truth observation without needing verbose logging at all.
- The exact meaning of each command byte (`0x51`/`0x53`/`0x40`/`0x41`/
  `0x52`) and the two distinct broadcast registers written before the
  `53 05` polls (`0xa8`, `0x18`) aren't decoded yet — captured
  faithfully, not yet explained. Worth cross-referencing against
  public BM13xx protocol writeups if the reimplementation attempt
  in [Next steps](#next-steps) doesn't work purely from the captured
  bytes alone.

### Round 5: chip discovery actually works — 77/77 chip responses, on real hardware

Reimplemented `s19k_probe.rs` with the corrected sequence from Round
4 (VersionMask x3 → InitControl → MiscControl → ChainInactive x3 →
SetChipAddress sweep for all 77 chips → the 32 captured per-chip
domain config writes, replayed verbatim → discover, sent last). Reused
Mujina's existing typed `Command`/`Register` machinery everywhere it
already matched the real captured register addresses cleanly
(`RegisterAddress::InitControl`/`MiscControl`/`IoDriverStrength`/
`UartRelay` all already existed and decoded the captured bytes
correctly via `Register::decode`) — only the specific per-chip
domain-write subset/values are hardcoded verbatim rather than derived
from a general rule, since their exact semantics still aren't
understood (see Round 4's "not yet explained" notes).

**First test, chain 3 alone at whatever residual ~13V was left on the
PSU rail from a prior session: still zero responses.** Two more
variables got isolated one at a time before it worked:

1. **A real reset pulse and settle delay**, not just "set the
   chain-enable GPIO high": drive it low first (200ms), then high,
   then wait 2s before any UART traffic — matching the ~2s gap
   `bosminer`'s own log shows between "Resetting hash board" and
   "Initializing hashchain". Still zero responses alone, but kept
   since it's a real correction (Round 1's version just set the GPIO
   high unconditionally with no low-first edge).
2. **All three chain-enable GPIOs asserted together, not just the one
   being tested.** This turned out to be the actual missing piece, and
   was cleanly isolated with a direct A/B test: chain 3 alone, at a
   confirmed real ~15.25V, still got **zero** responses; chains 1+2
   also enabled, same ~15.25V, same command sequence → **77/77
   responses**. A second confirmatory run on chain 1 (all three
   enabled, same hardcoded chain-3-captured domain-config table
   replayed unchanged) also got clean 77/77 — proving both that "all
   three enabled" is the real requirement (not a chain-3 fluke) and
   that the per-chip domain config table generalizes across chains
   (all three hashboards share identical topology, as expected from
   board symmetry). Real ~15.2V voltage alone, without all three
   chains enabled, was *not* sufficient — the isolation specifically
   rules that out, though the reverse (all three enabled, at only the
   old ~13V residual voltage) wasn't separately tested, so a residual
   voltage dependency isn't fully excluded.

```
$ s19k-probe 3
...
Sending discover_chips (broadcast ChipId read) -- as the LAST step, matching the real capture...
  chip: resp_addr=0x00 chip_id=1366 (BM1366) core_count=0 reg_address=0x00
  chip: resp_addr=0x02 chip_id=1366 (BM1366) core_count=0 reg_address=0x02
  ... (75 more) ...
  chip: resp_addr=0x98 chip_id=1366 (BM1366) core_count=0 reg_address=0x98
Chain 3: 77 chip response(s), 847 raw byte(s) received in 2s
MATCHES expected 77 chips/chain.
```

**The chips self-report as `BM1366`, not `BM1362`** — decoded from
real received `ChipId` bytes (`0x13 0x66`, matching
`ChipType::BM1366`'s existing definition in `protocol.rs`, not a
decode bug). This had been assumed wrong throughout the whole
investigation, inherited from `REFERENCE.md`'s S19j Pro capture (a
real BM1362 board) being the closest available reference for "the
same chip family." It's very plausibly *why* blindly following that
reference's sequence never worked: BM1366 is a newer chip
generation (used in e.g. some S19XP-family units) that may share
BM1362's wire *frame format* closely enough to pass Round 2's
byte-for-byte verification, while requiring a genuinely different
*bring-up sequence* — which is exactly the shape of the actual fix.
Every downstream reference to "BM1362" in this document and the
codebase (`board/antminer_s19k_am3.rs`'s module docs, `s19k_probe.rs`'s
header, etc.) should be corrected to BM1366 as that work proceeds;
not renamed everywhere in this pass, to keep this diff reviewable.

**One real cost paid getting here, documented rather than glossed
over**: extending `s19k-trace` with `ioctl`/`TCSETS` decoding (to
settle a since-abandoned "wrong baud rate" hypothesis — the real baud
turned out to already be 115200, exactly what every round had been
using) triggered a genuine `bosminer` crash on the *traced* process
mid-run (`Serial/W/1: Failed to set baudrate 115200, terminating:
Interrupted system call (os error 4)`) — almost certainly the
tracer's added `/proc/pid/mem` read on every `ioctl` stop introducing
enough latency/signal interaction to occasionally return `EINTR` to
the tracee on that specific syscall, which `bosminer` treats as fatal
for that worker thread rather than retrying. Caught immediately via
the standing post-experiment fan/discovery health check, and resolved
by simply restarting the supervised instance — full recovery,
77/77 discovery, healthy fans, confirmed with fresh (non-stale)
telemetry within a minute. No lasting effect on the hardware or on
chain 3's own health (its *own* next real bring-up, moments later,
discovered 77/77 normally) — but a real, reproducible limitation of
the tracer worth knowing about before using it again: decoding
`ioctl` args on a live, production-critical tracee carries real risk
that plain `open`/`write` decoding (which never touches the tracee's
memory on a *blocking* syscall in a way that could race a fast
control-plane call like `TCSETS`) didn't.

**What's not yet done**: only chains 1 and 3 were tested (chain 2 by
symmetry is very likely fine, but wasn't separately confirmed); the
corrected sequence lives only in the standalone `s19k_probe.rs` tool,
not yet in the real board driver; and the per-chip domain-config
values are still replayed verbatim rather than understood — see
[Next steps](#next-steps).

## EEPROM corruption incident and repair

Documented in full for transparency, since this will eventually be a
public repo and the incident is a genuinely useful lesson, not
something to bury.

**What happened**: while investigating the PSU bus (before finding
the real bit-banged answer above), `apw12-psu-tool scan` was run
against its default address range (`0x50`-`0x5F` on `/dev/i2c-1`) to
compare its behavior at a known-live address against its behavior at
the suspected PSU address. `scan`'s `exchange()` doesn't just probe
— it performs a real `I2C_SMBUS_WRITE_BYTE_DATA` write to register
`0x11` for every byte of its outgoing frame, at every address it
scans, including the three real, populated hashboard EEPROM addresses
(`0x50`/`0x51`/`0x52`). Each byte got written to the *same* register
offset in sequence, so the last frame byte (`0x55`) permanently
overwrote whatever was previously stored at EEPROM offset `0x11` on
all three chips.

**Effect**: `bosminer`'s hashboard model detection broke from the
very next restart onward (`{{ERR:E2}} Model detection failed: ...
No successfully parsed hashboard EEPROMs`). It fell back to a dummy
backend with the PSU explicitly disabled and hashboards never
enabled. **The miner was not actually hashing for a period during
this session**, despite the `bosminer` process staying alive the
whole time — a real, if minor and fully-recoverable, mistake.

**Diagnosis**: caught by diffing `Raw Hashboard #N data:` log lines
in `bosminer.log`. Every boot going back to 2026-07-31 (weeks of
history, including two clean restarts earlier in this same session)
consistently showed the same byte at offset 17 (`0x11`) per board;
every boot after the `scan` command consistently showed `0x55` at
that same offset and nowhere else. That consistency — a single fixed
offset, a single fixed new value, identical across three independent
chips — is what made this fully diagnosable rather than a mystery.

**Repair**: the exact original values were recoverable from that same
historical log data, with high confidence given weeks of consistent
readings:

| EEPROM | Address | Restored value |
|---|---|---|
| Hashboard #1 | `0x50` | `0xe7` |
| Hashboard #2 | `0x51` | `0x54` |
| Hashboard #3 | `0x52` | `0xbb` |

Applied with individually-verified `i2cset`/`i2cget` round-trips (not
the buggy third-party tool). Confirmed fully repaired by restarting
`bosminer`: `Detected hashboards: [1, 2, 3]`, real board names and
serial numbers recovered, all three chains discovered 77/77 chips,
chain-enable GPIOs back to their normal mining values, tuner ramping
frequency normally. **Fully back to healthy, normal mining** — this
was confirmed multiple times across the rest of the session, most
recently right before the PSU protocol verification above.

**The lesson**: a command whose *effect* isn't independently verified
read-only shouldn't be run against an address known to host live,
meaningful device state — not because "it's probably an EEPROM/
sensor so it's probably safe," which turned out to be exactly wrong
here. `apw12-psu-tool scan`'s documentation doesn't mention that it
writes, and it isn't obvious from the command name. Prefer plain
`i2cget` (confirmed read-only by its own semantics) over unfamiliar
third-party tooling when probing addresses that matter, or read the
tool's source first — which would have caught this immediately here;
`exchange()`'s use of `write_byte_transaction` for every scanned
address is right there in `linux_i2c.rs`/`apw12-psu-tool.rs`.

## Native hw_trait implementations and the first working board driver

With chip discovery stuck (see above), moved to filling the
architecture gap the earlier reading of Mujina's source identified:
no native Linux GPIO/I2C `hw_trait` implementations existed, and
`VirtualBoardDescriptor` dispatch only worked for `cpu_miner`. Both
are now real, and there's a real (if partial) board driver running
on the hardware.

### `src/linux_hw/` — native Linux hw_trait backends

Three implementations, none pulling in a new crate dependency (hand
-rolled against `nix`'s raw `libc::ioctl` and plain `tokio::fs`,
matching what the probe binaries had already proven working):

- **`gpio.rs` — `SysfsGpio`/`SysfsGpioPin`.** `hw_trait::Gpio::pin`
  takes a `u8`, but this SoC's mining-relevant GPIO lines are
  numbered well past 255 globally (e.g. line 454). `SysfsGpio` binds
  to one controller's base line number at construction (411 for the
  `periphs-banks` controller), so callers address pins by their
  offset *within that controller* instead — line 454 becomes offset
  43. Async via plain `tokio::fs::write`/`read_to_string`.
- **`i2c.rs` — `LinuxI2c`.** A real `/dev/i2c-N` bus via
  `ioctl(I2C_SLAVE)` + plain `read()`/`write()`. Since `hw_trait::I2c`
  methods take `&mut self` and each call needs to run inside
  `tokio::task::spawn_blocking` (a raw ioctl can't cross an `.await`
  cleanly), every call clones the file descriptor first
  (`File::try_clone`) — safe, since `I2C_SLAVE`'s address-selection
  state lives on the shared underlying open-file-description, not
  per-fd-number, so a freshly cloned handle still sees whatever
  address the last call selected.
- **`bitbang_i2c.rs` — `BitBangI2c`.** Software I2C over two
  arbitrary GPIO lines, generic (not PSU-specific) — for buses that
  aren't a real Linux device at all. Runs each whole transaction
  (not just each byte) inside `spawn_blocking`, since the
  microsecond-scale bit timing bit-banging needs can't survive async
  executor scheduling jitter between individual GPIO writes.

`write_read` on `BitBangI2c` gets a **true repeated START** (since
the bit-bang code drives `START`/`STOP` directly), unlike `LinuxI2c`'s
`write_read`, which is two separate transactions with a `STOP`
between them — fine for the register-addressed devices on this bus
(TMP75, EEPROM), but worth knowing if a future device needs a real
repeated start over the *real* I2C bus (would need the combined
`I2C_RDWR` ioctl instead of plain `read()`/`write()`).

### `peripheral/apw12.rs` — a proper, reusable PSU driver

The APW12 framed protocol (ported from `psu_bitbang_probe.rs`, which
itself ported it from skot's `apw12-psu-tool`) turned out to map
directly onto plain `I2c::write`/`I2c::read` calls — each frame byte
is already its own single-byte write transaction, and the response is
already read back one byte at a time. So instead of living in a
throwaway probe, it's now `Apw12<I>`, generic over any `I2c`
implementation, alongside the project's other peripheral drivers
(`tmp1075.rs`, `tmp451.rs`, `tps546.rs`) — the same driver works
unchanged whether wired to a real I2C bus or `BitBangI2c`. Includes
unit tests checked against real captured hardware response bytes
(`measure-voltage` → 15.1683V, `get-voltage` DAC 0xE2 → 12.16V, both
from earlier live sessions).

### `board/antminer_s19k_am3.rs` — a real, partial board driver

Registers via `VirtualBoardDescriptor` (`device_type:
"antminer_s19k_am3"`), gated behind a new
`MUJINA_ANTMINER_S19K_AM3_ENABLE` env var (documented in
`env_help.rs`), reachable through the same additive
`TransportEvent`/`daemon.rs`/`backplane.rs` plumbing pattern
`cpu_miner` already used (now proven to generalize to a second board
cleanly).

What it wires up, all confirmed live on real hardware (smoke test:
`bosminer` stopped, `mujina-minerd` run by hand with the env var set):

- **Chain enable/reset and presence-detect GPIO** via `SysfsGpio` —
  `presence=[true, true, true]` logged correctly for all three real
  hashboards.
- **Six TMP75-compatible temperature sensors** via `LinuxI2c` +
  the existing `Tmp1075` driver (skipping its `init()` device-ID
  check, since bosminer's own log names these
  `Lm75BCCnCopy` — a basic LM75 clone without TI's ID register, not
  a genuine TMP1075).
- **PSU voltage telemetry** via `BitBangI2c` + the new `Apw12` driver.

All of it verified through the real REST API
(`GET /api/v0/boards`, reached by SSH-port-forwarding `127.0.0.1:7785`
off the miner since its BusyBox has no `curl`/`wget`/`nc` at all):

```json
{
  "name": "antminer-s19k-am3",
  "temperatures": [
    {"name": "chain1-inlet", "temperature_c": 25.6875},
    {"name": "chain1-outlet", "temperature_c": 29.125},
    ...
  ],
  "powers": [
    {"name": "psu", "voltage_v": 0.029539648, ...}
  ],
  "threads": []
}
```

The temperatures are real, sensible ambient readings. The PSU voltage
(~0.03V) correctly reflects that output is off — matching the
deliberate design choice below.

**PSU output was originally deliberately left disabled by default** —
true when this section was first written, no longer true as of
[Round 6](#round-6-discovery-wired-into-the-real-board-driver) below,
once there was a working chip protocol to justify powering the
hashboards for real. Kept here as the accurate historical record of
why that choice was made at the time, not because it still describes
current behavior.

**No hash threads yet, by design** — `threads: Vec::new()` with a
`warn!()` explaining why, exactly matching `emberone00.rs`'s own
precedent for the same underlying reason. As of Round 6 the reason
has changed, though: not because chips are unreachable (they aren't —
see below), but because there's no verified BM1366 mining-ready
register sequence and `BM13xxThread` wiring yet.

Smoke test cleanup was clean too: `SIGTERM` → `Board stopped` logged,
chain-enable GPIOs correctly released back to their disabled value,
`bosminer` restarted and resumed normal mining immediately after,
fans confirmed healthy throughout (per the
[fan safety rule](#risk-posture)).

### Round 6: discovery wired into the real board driver

Everything above (PSU left off, no chip protocol, `threads:
Vec::new()`) was true right up until chip discovery actually started
working (Round 5). With a real, working sequence in hand, `create_board()`
now does real hardware bring-up as part of registering the board, not
just what was safe to do with zero chip protocol:

1. **Powers the PSU for real**: disables its comms watchdog, drives
   `PSU_nEN` low, and gradually ramps output voltage to the confirmed
   real ~15.2V target (Round 3) in 0.5V steps — not a sudden jump to
   full voltage on a rail that may not have been driven in a while.
2. **Resets all three chains together**, not independently — Round
   5's key finding, now load-bearing in the real driver: a single
   chain enabled alone never responds, even at correct voltage.
3. **Runs the corrected per-chain discovery sequence** (VersionMask
   x3 → InitControl → MiscControl → ChainInactive x3 → SetChipAddress
   sweep → domain config writes → discover, sent last) over each
   chain's real UART, using Mujina's actual typed `Command`/`Register`
   machinery and `FrameCodec` decode — not raw bytes, the real
   protocol stack.

**Verified end-to-end on real hardware, through the actual daemon**
(`bosminer` stopped, `mujina-minerd` run by hand with
`MUJINA_ANTMINER_S19K_AM3_ENABLE=1`):

```
INFO board::antminer_s19k_am3: Hashboard presence detect
               presence=[true, true, true]
INFO board::antminer_s19k_am3: Chain discovery OK
               chain=1, chips=77
INFO board::antminer_s19k_am3: Chain discovery OK
               chain=2, chips=77
INFO board::antminer_s19k_am3: Chain discovery OK
               chain=3, chips=77
WARN board::antminer_s19k_am3: BM1366 register-init/frequency-tuning sequence not yet solved for this board (chip discovery works -- see HANDOFF.md's Round 5) -- registering with no hash threads
INFO backplane: Board started.
               board=Antminer S19K Pro (AM3), serial=antminer-s19k-am3, threads=0
```

Bring-up failures are deliberately non-fatal — a PSU hiccup or one
chain not responding logs a warning and moves on, rather than taking
down temperature/PSU telemetry (which are independently useful and
don't depend on discovery succeeding). Nothing downstream depends on
discovery succeeding *yet* either, since hash threads still aren't
created either way — see below.

**A real issue found and fixed during this testing, not glossed
over**: the bit-banged PSU I2C bus occasionally returned a NAK
partway through the voltage ramp — consistent with bus noise this
document already flagged (Round 3's note about voltage readings that
"bounce unexpectedly," attributed to bit-bang timing jitter), not a
logic bug in the ramp itself. A single `set_voltage` call from a
fresh CLI process (`psu-bitbang-probe`) was reliably reproduced as
reliable; a longer in-process sequence of many calls back-to-back
occasionally wasn't. Fixed with a bounded retry (`set_voltage` is
idempotent, so retrying the same value is safe) — confirmed on real
hardware to recover cleanly from an actual NAK and complete the ramp.

**A process mistake worth recording, not just the fix**: while
diagnosing the NAK, a still-running `mujina-minerd` test instance
(whose periodic PSU telemetry monitor task was still polling in the
background) was left alive while `psu-bitbang-probe` was run manually
against the same bit-banged I2C bus concurrently — precisely the
two-masters-on-one-bus pattern that caused the
[EEPROM corruption incident](#eeprom-corruption-incident-and-repair)
earlier in this project. Caught immediately, and this time genuinely
harmless (verified straight after: DAC setpoint held a sane in-range
value consistent with the ramp's own progress, PSU output was
confirmed physically off, no corruption) — but a reminder that this
specific hazard is easy to reintroduce by habit even with it fully
documented once already. Always confirm a prior test process has
fully exited (`ps`, not just "I sent the kill command") before running
anything else against the PSU bus.

**What's not yet done**: `threads: Vec::new()` remains, for a
different reason than before. It's not that chips are unreachable
now — it's that:

- `BM13xxThread`'s lazy `initialize_chip` (in `asic/bm13xx/thread.rs`)
  is currently hardcoded for exactly one chip at address `0x00`, tuned
  specifically for BM1370 (Bitaxe Gamma's chip) — wrong shape (one
  chip, not a 77-chip sweep) and wrong values (its `MiscControl`/
  `InitControl`/frequency-ramp constants don't match what was actually
  captured for BM1366 in Round 4/5) for this board. Reusing it as-is
  isn't an option; it needs to become pluggable per chip family/board
  topology before a real `BM13xxThread` can be constructed for this
  board without regressing Bitaxe's working path.
- There's no verified BM1366 register-init/frequency-tuning sequence
  beyond what gets chips to respond to *discovery* — actual sustained
  mining needs PLL/frequency ramp, TicketMask, and NonceRange tuning
  that hasn't been captured or tested at all yet (see
  [Next steps](#next-steps)).

### Round 7: real hash threads, real pool connection — but no confirmed hashing yet

Generalized `BM13xxThread`'s chip-init path (`asic/bm13xx/thread.rs`)
from the hardcoded single-BM1370-chip sequence into a pluggable
`ChipInitStrategy` enum — `Bm1370Single` preserves Bitaxe's exact
existing behavior unchanged, `Bm1366Chain` adds a real 77-chip
daisy-chain bring-up (address sweep, domain config, the real captured
post-discovery `Core`/`PllDivider`/`TicketMask` broadcasts from Round
4's trace) for this board. `board/antminer_s19k_am3.rs` now
constructs one real `BM13xxThread` per chain, sharing one idempotent
chain-enable/reset state (`SharedChainEnable`) across all three —
necessary because a single chain's thread might get its first job
assignment well before or after the others, at whatever time the
scheduler gets around to it, but Round 5 already established all
three chains must be reset *together*.

**What's genuinely real and verified end-to-end on hardware**:
connects to the real Stratum pool
(`stratum+tcp://pool.256foundation.org:3333`, worker
`<your-npub>.mujina-s19k-pro`),
subscribes, gets version-rolling authorized, receives real jobs, and
dispatches correctly-framed 88-byte `JobFull` commands (confirmed via
wire-level `TX BM13xx` tracing, proper CRC16 framing, `ntime`-rolling
every second) to all three real chains. This is a real Bitcoin miner
talking to a real pool over real hardware — genuinely new ground.

**What's not yet working: actual chip hashing.** Across several
minutes of real job dispatch (both against the live pool and,
separately, the built-in dummy job source), **zero `Nonce` responses
were ever observed** — confirmed by grepping for wire-level `RX
BM13xx` trace lines, which stayed frozen at exactly 231 (77×3, the
discovery-phase `ChipId` responses) for the entire test regardless of
how long jobs kept flowing. Two real hypotheses were tested and
ruled out, not just theorized about:

1. **Missing `NonceRange`.** The real capture never showed a
   broadcast `NonceRange` write at all (unlike `VersionMask`/
   `InitControl`/`MiscControl`/etc., which all appear clearly).
   Added one anyway, reusing BM1370's own fixed value (`0xB51E0000`,
   unpartitioned — every chip in the chain searches the same
   subrange redundantly rather than getting its own slice). No
   change: still zero responses.
2. **PLL frequency too low.** The only `PllDivider` value ever
   captured on the wire (`40 a8 02 65`) decodes to *exactly* 50.0MHz
   via the same `calculate_pll_for_frequency` formula BM1370 already
   uses and has reference-tested — too clean a match to be
   coincidence, so the formula itself is trustworthy for this chip
   family too. But `bosminer`'s own tuner config logs
   `on_start_target_percent: 66` (of a `hashrate_target:
   TeraHashes(120.0)` — i.e. ~79TH/s from the start, not a slow ramp
   from near-zero), strongly suggesting 50MHz is a transient
   bring-up value, not `bosminer`'s real operating frequency. Tested
   a computed, still-conservative 200MHz (well under the chip's own
   confirmed-safe real ceiling — `bosminer` itself runs chips at
   63–66°C at full ~670MHz nameplate, so 200MHz carries essentially
   no thermal risk) via the same trusted formula. No change: still
   zero responses, though board temps did rise measurably (26°C →
   32°C), consistent with *some* real electrical activity at the
   higher clock even without confirmed hashing.

Both experiments are preserved as ruled-out ideas in
`initialize_chip_bm1366_chain`'s doc comment, not silently reverted —
the code still runs at the confirmed-real 50MHz value (no reason to
keep the unverified 200MHz since it demonstrated no advantage), but
the doc comment is explicit that frequency was *tested* as a
hypothesis and didn't pan out, not that it's a settled non-issue.

**A real dead end investigated and explained, not just abandoned**:
the natural next move was to get a *longer* or *cleaner* ptrace
capture of `bosminer`'s real post-discovery behavior, specifically
looking for more `PllDivider` writes beyond the single value already
found. Re-analyzing the full 12MB Round 4 trace more carefully found
**zero** additional `PllDivider` writes anywhere — broadcast or
per-chip-addressed — across the entire capture, despite it spanning
~250 seconds of trace-clock time (several real bring-up/retry
cycles). The most likely explanation: `s19k-trace`'s ~5.6× slowdown
overhead (established in Round 4) may itself have kept `bosminer`
from ever reaching genuine steady-state operation during any of that
capture — a heavily ptrace-slowed process could plausibly trip its
own internal "chip should respond by now" timeouts and keep
restarting the bring-up cycle rather than settling into real mining,
which would fully explain why only the same initial bring-up default
ever appears, repeated identically on every retry. If true, this is
a real limitation of ptrace-based tracing for capturing *later*
bring-up phases specifically (as opposed to the *early* discovery
phase Round 4/5 captured successfully, which happens fast enough
that even 5.6× slowdown didn't prevent it from completing) — not
something fixable by waiting longer or re-running the same approach.

**What remains genuinely open** (see [Next steps](#next-steps)):
whether real hashing needs the baud switch to 3,125,000 (untested —
switching requires reconfiguring the host tty mid-connection, not
just sending the chip-side register write), the skipped per-chip
calibration writes (`InitControl`/`MiscControl`/`Core`/`PllDivider`
at specific individual chip addresses, distinct from the broadcast
`domain_config` sweep — plausibly load-bearing rather than cosmetic
tuning), some other still-unidentified register or command, or a
capture limitation meaning the real answer was never actually
observed at all. This is now genuine unsolved protocol-reverse-
engineering territory, not a matter of trying one or two more
parameter combinations.

**No lasting hardware effect from any of this**: `bosminer` restarted
cleanly after every test in this round, discovering 77/77 chips
immediately each time; fans and temperatures were monitored
throughout per the [fan safety rule](#risk-posture) and stayed
healthy at every check (PWM confirmed at 100% fail-safe duty
whenever `bosminer` wasn't running to drive it, board temps never
exceeded ~32°C even during the 200MHz experiment).

### Round 8: baud switch tested (no change); a real corruption source found and removed

Two more concrete experiments, continuing directly from Round 7's
still-open "chips never return a `Nonce`" question.

**Real baud switch, tested and inconclusive.** Added a `BaudControl`
trait to `asic/bm13xx/thread.rs` (implemented for
`transport::serial::SerialControl`, threaded through
`ChipInitStrategy::Bm1366Chain`) that sends the exact captured
`UartBaud` register value (`0x00003011` — not this codebase's
`BaudRate::Baud3M`/`Baud1M` constants, which were captured from a
different chip/board and don't match this board's real wire bytes)
and then reconfigures the host tty to `bosminer`'s confirmed real
3,125,000 operating baud. Mechanically this works cleanly —
communication continues normally post-switch, jobs keep dispatching —
but it did not produce a single `Nonce` response either. Kept in the
driver (it's a real, defensible piece of the actual captured
sequence, and does no harm), but ruled out as *the* fix.

**A real, concrete corruption source found.** Extended `s19k_probe.rs`
with an incremental diagnostic: send one register write from the real
captured post-discovery sequence, immediately re-discover, and report
per-step — rather than sending the whole sequence and only checking
at the end. This isolated something Round 7's "does the sequence
still respond at all" check (which only checked *after* the full
sequence) had missed entirely: the broadcast **`Core` register write
corrupts communication for an extended, non-deterministic period
afterward**. A bare register read shortly after sending it returns
not silence but a *flood* of garbled data — thousands of raw bytes
(9,740–18,889 across different runs, vs. the clean 847 bytes/77
responses a real discovery produces) that fail to decode
(`Invalid register address: 0x40` and similar). Two follow-up
findings sharpen this:

- **A longer fixed delay after `Core` made it *worse*, not better**
  (0 decoded responses and 18,763 garbled bytes at a 1-second settle,
  vs. 12 responses and 9,740 bytes at a 50ms settle) — ruling out
  "just needs more time to settle" and pointing at something more
  structural (a possible undocumented side effect of the `Core`
  write, plausibly related to baud/timing, though not confirmed).
- **It recovers anyway after a few more commands pass**, converging
  back to a clean 77/847 by the time `PllDivider` is reached,
  regardless of how long or short the delays were — consistent with
  subsequent traffic (not elapsed time) being what actually resyncs
  the bus, though the exact mechanism isn't understood.

**Why this matters even though it didn't solve Round 7's mystery**:
Round 5/6's proven 77/77 discovery sequence *never included the
`Core` write at all* — it was only added in Round 7/8's
`initialize_chip_bm1366_chain`, while trying to be more complete for
real hashing (reasoning that discovery alone doesn't prove mining
readiness). Given it demonstrably corrupts communication with no
demonstrated benefit, it (along with the adjacent `AnalogMux`/
broadcast-`IoDriverStrength` writes sent right alongside it in the
real capture) was removed from the driver — a real, defensible
simplification back toward the proven-working sequence, not a new
guess. **Tested end-to-end after removing it: still zero `Nonce`
responses.** So this wasn't *the* root cause of Round 7's silence
either — but it's a genuine, reproducible finding worth keeping
(a real corruption mode now understood well enough to avoid, and a
diagnostic technique — incremental per-step re-discovery — proven
valuable enough to reuse for whatever's investigated next).

**What's still completely open**: four real hypotheses have now been
tested and ruled out (missing `NonceRange`; PLL frequency too low;
wrong baud; the `Core` write corrupting the bus) without producing a
single confirmed `Nonce`. The skipped per-chip addressed calibration
writes and a cleaner/longer real capture of `bosminer`'s actual
steady-state behavior (see Round 7's write-up on why the existing
Round 4 trace likely never captured it) remain the most promising
untried leads. This is genuine unsolved territory — see
[Next steps](#next-steps).

No lasting hardware effect from this round either: `bosminer`
restarted cleanly every time, fans confirmed healthy at every check
(2,300–2,800+ RPM, `broken fans` reading 0 throughout), no elevated
temperatures observed.

### Round 9: the `Core` corruption fully explained — a real endianness bug, fixed

Round 8 removed the `Core` register write as a workaround without
understanding *why* it corrupted communication. This round found out
why, with a real, verified root cause and fix — not another
workaround.

**The corrupted bytes were never noise.** A careful byte-level
analysis (reimplementing the exact CRC5 algorithm in Python and
walking the captured post-`Core` byte stream exactly like the real
decoder does) found **885 perfectly gapless, CRC5-valid 11-byte
frames** — a completely regular, structured stream, not garbage. Every
frame decoded as a `ReadRegister`-shaped response for an unrecognized
register address `0x40`, from multiple real chip addresses (0, 2, 4,
… 30), each with its own slowly-incrementing counter value. This was
real chip output the whole time — our own code just didn't understand
what it meant.

**The actual bug**: `Register::decode()` always interprets wire bytes
as little-endian, but `Core`'s own `encode_data` uses big-endian
(`put_u32`, not `put_u32_le`) — a real asymmetry already present in
the codebase, undocumented as a decode-side gotcha. Every `Core`
register write sent throughout Rounds 5–8 was therefore built from a
**wrongly-decoded value**, and once re-encoded through `Core`'s real
big-endian path, sent a **malformed `CORE_MAILBOX` command** (see
`asic/bm13xx/REFERENCE.md`'s `0x3C - CORE_MAILBOX` section — a 32-bit
indirect-addressing register for per-core config, with `all`/`wr`/
`rd`/`core_id`/`reg`/`value` bitfields) — not the real captured one.

**Confirmed by cross-referencing against `REFERENCE.md`'s own
documented values**: decoding the real captured `Core` bytes as
big-endian instead (matching `encode_data`'s stated behavior)
produces `0x80008540` and `0x80008020` — which decode via the
documented `CORE_MAILBOX` bitfield to *exactly* "broadcast write core
register 0x05 (clock select) = 0x40" and "broadcast write core
register 0x00 (clock delay) = 0x20", both **exactly matching
`REFERENCE.md`'s own documented BM1366 bring-up values** for those
registers. The wrong little-endian interpretation, by contrast,
produced a semantically garbage command (`wr=0`, non-zero `num` field
that's "zero in every observation" per the docs, an out-of-range
`core_id`) — fully explaining why the mailbox state machine got
confused and chips started streaming unexpected telemetry.

**Fixed and verified on real hardware**: `Register::decode` now
special-cases `Core` to use big-endian. The incremental per-step
diagnostic that previously showed a 77→12 chip regression right after
`Core` now shows a **clean 77/77 at every single step**, through
`AnalogMux`/`IoDriverStrength`/`PllDivider`/`TicketMask`/`NonceRange`
— the corruption is completely gone. Restored the `Core`/`AnalogMux`/
`IoDriverStrength` writes to `initialize_chip_bm1366_chain` now that
they're correctly encoded, rather than leaving them removed.

**Still does not produce a `Nonce`, tested end-to-end.** This closes
out a real, well-understood bug — five real hypotheses now ruled out
total (`NonceRange`, PLL frequency, wrong baud, and now both the
`Core` corruption *and* its root cause) — but the core "why don't
chips hash" question remains open. Two things worth trying next given
this fix, not yet attempted for lack of time: `REFERENCE.md`'s
`CORE_MAILBOX` section documents a **third write this driver has
never sent at all** — "0x02 core enable: 0xAA on every model,
per-chip pass only" — literally enabling the hashing cores, addressed
per-chip rather than broadcast; and the broader bring-up pattern
`REFERENCE.md` describes ("first broadcast, then repeated per chip
with core enable appended") suggests the two `Core` writes need a
per-chip pass too, not just the broadcast ones already sent.

No lasting hardware effect: `bosminer` restarted cleanly, fresh
discovery confirmed 77/77 on all three chains, fans healthy at every
check (2,300–2,600+ RPM, `broken fans` reading 0 throughout).

### Round 10: the documented per-chip core-enable pass, added and tested — still silent

Implemented the missing piece Round 9 identified: `REFERENCE.md`'s
`0x3C - CORE_MAILBOX` section documents a per-chip pass (addressed to
each chip individually, not broadcast) that repeats the clock-config
writes and appends a third write this driver had never sent at
all — `0x02 core enable: 0xAA on every model, per-chip pass only`.
Added a full 77-chip × 3-write pass (`initialize_chip_bm1366_chain`,
right after the existing broadcast `Core`/`AnalogMux`/
`IoDriverStrength` writes).

**Tested end-to-end on real hardware: still zero `Nonce` responses.**
Discovery and job dispatch continue to work cleanly. One notable
signal: board temperatures stayed completely flat (25–28°C, identical
to before this change) throughout — if cores were newly drawing real
hashing power, some temperature rise would be expected even at the
conservative ~50MHz clock (`bosminer`'s own real chips reach 63–66°C
at full ~670MHz). This suggests core enable alone isn't sufficient to
get cores meaningfully active, or something upstream is still
blocking it. Kept in the driver regardless — it's a real, documented
gap being closed, not a guess, and having chip-level core enable
genuinely correct is a prerequisite for anything working, whatever
else turns out to also be missing.

**A dead end investigated and ruled out in this round**: briefly
suspected `InitControl` (`0xA8`, `REFERENCE.md`'s `SOFT_RESET_CONTROL`)
might have the same little-endian/big-endian asymmetry bug as `Core`,
since `REFERENCE.md`'s BM1370 example value (`0x00070000`) looks
completely different from our own captured S19K value (`0x00000700`
when decoded little-endian). Self-corrected before acting on it:
unlike `Core`, `InitControl`'s encode and decode are internally
consistent (both little-endian, no special-case comment), and the
little-endian-decoded value is independently verified to be the exact
real byte sequence `bosminer` sends during its own successful
bring-up (Round 4's capture) — the REFERENCE.md BM1370 value is
simply a different chip using a different value for the same
register, not evidence of a bug. Recorded here so a future session
doesn't re-suspect and re-investigate the same already-ruled-out idea.

Also checked
[256foundation/asic-rs](https://github.com/256foundation/asic-rs) (a
sibling project from the same community) on the chance it had
low-level BM13xx protocol code to cross-reference — it doesn't. It's
a fleet-management/monitoring library that talks to miners' *existing*
web APIs (`bosminer`'s, CGMiner's, etc.) for stats/fan/board-layout
reporting, not a from-scratch chip driver. It does correctly model
this exact hardware (`AntMinerModel::S19KPro { fans: 4, boards:
[77, 77, 77] }`, matching our own confirmed hardware map exactly) but
has no `SetChipAddress`/`TicketMask`/`PllDivider`/register-level code
anywhere in the repo — not useful for this specific investigation.

No lasting hardware effect: `bosminer` restarted cleanly, fresh
discovery confirmed 77/77 on all three chains, fans healthy at every
check (5,300–5,700+ RPM, `broken fans` reading 0 throughout).

### Round 11: the full documented per-chip pass, tested against the real pool — still silent

Closed out the last piece of `REFERENCE.md`'s documented "per-chip
pass" step: `InitControl`/`MiscControl` now also get resent addressed
to each of the 77 chips individually (immediately before the
`CORE_MAILBOX` per-chip pass Round 10 added), using the same values
already verified real for this board's broadcast phase — the least
speculative option available, since this board's real *per-chip*
values were never captured.

**Tested end-to-end against the real Stratum pool this time**
(`pool.256foundation.org`, not the dummy job source) over roughly a
minute of continuous runtime: discovery, pool subscribe/authorize,
and job dispatch all worked cleanly throughout. **Still zero real
`Nonce` responses** — a naive log grep for "Nonce" matched only the
`NonceRange` register name being sent, not an actual decoded
`Response::Nonce`; verified directly that none ever appeared. Board
temperatures rose only marginally (25–29.6°C), consistent with every
earlier round.

**Eight real hypotheses now ruled out across Rounds 7–11**: missing
`NonceRange`; PLL frequency too low (tested at two different values);
wrong baud (tested with a real host+chip switch); the `Core` write
corrupting the bus (root-caused to a real endianness bug and fixed);
the missing per-chip `CORE_MAILBOX` core-enable write (added); and
now the missing per-chip `InitControl`/`MiscControl` pass (added).
Every register write `REFERENCE.md` documents as part of BM1370's
bring-up has now been sent in some form. The chips discover cleanly,
accept every command without error, and dispatch real jobs without
any decode failures — communication itself is unambiguously solid.
What's missing is either a BM1366-specific detail `REFERENCE.md`'s
BM1370-focused documentation doesn't cover, a value this board's real
firmware uses that differs from BM1370's documented ones (the same
gap that made the `CORE_MAILBOX` writes need this board's *own*
captured bytes rather than BM1370's directly), or something structural
neither register content nor addressing explains.

**Where this leaves the investigation**: further blind-guessing at
individual register values has clearly reached diminishing returns —
eight real, well-reasoned hypotheses, each grounded in either real
captured bytes or documented reference behavior, have all come back
negative. The two remaining paths that seem likely to actually move
this forward are qualitatively different from what's been tried:
1. **A real capture of a chip's actual *read* responses** during a
   genuinely successful bring-up, not just the writes `bosminer`
   sends — every capture so far (Round 4 onward) only ever recorded
   TX traffic; the chips' own replies during real per-chip
   configuration have never been observed. Extending `s19k-trace` to
   decode `read()` syscalls (a known, previously-noted limitation)
   would show what a working chip actually *says* at each step,
   rather than requiring everything to be inferred from what
   `bosminer` sends.
2. **Directly comparing this board's real captured per-chip
   `InitControl`/`MiscControl`/`CORE_MAILBOX` values** (not yet
   captured — every round so far only replayed the *broadcast* phase
   verbatim) against what Round 10/11 guessed by reusing the
   broadcast values. If `bosminer`'s real per-chip values differ from
   its broadcast ones (as BM1370's documented sequence does, adding
   "bring-up" bits), that's the most likely single remaining gap.

No lasting hardware effect: `bosminer` restarted cleanly, fresh
discovery confirmed 77/77 on all three chains, fans healthy at every
check throughout.

### Round 12: solved — real per-chip values extracted from the existing trace, and Mujina mines its first accepted share

Round 11 ended with two candidate next steps: capture real chip
*read* responses, or find this board's real captured per-chip
`InitControl`/`MiscControl`/`CORE_MAILBOX` values instead of reusing
the broadcast-phase ones. The second one didn't need a new capture —
the original Round 4 `/tmp/trace.log` (still on the miner, 12MB,
write-only via `s19k-trace`'s ptrace tracer) had never actually been
searched for *addressed* (non-broadcast) writes to those three
registers. Every prior pass through that trace only ever pulled out
the broadcast-phase values. A targeted grep for
`write(/dev/ttyS1...) [55, aa, 41, 09, <addr>, <reg>, ...]` finally
ran that search, and it turned up real, consistent, previously-unseen
data across all 77 chip addresses (`0x00`..`0x98`) on every chain:

- **Per-chip `InitControl`**: `00 07 01 f0` — genuinely different
  from the broadcast value (`00 07 00 00`). This matches
  `REFERENCE.md`'s description of the per-chip pass adding extra
  "bring-up" bits on top of the broadcast value, which Round 10/11's
  guess (reusing the broadcast value verbatim) missed entirely.
- **Per-chip `MiscControl`**: `f0 00 c1 00` — also genuinely different
  from the broadcast value (`ff 0f c1 00`).
- **Per-chip `Core` (`CORE_MAILBOX`) triplet**, in this exact real
  order — different from what Round 10 guessed:
  1. `80 00 80 20` (reg `0x00` clock delay = `0x20`)
  2. `80 00 82 aa` (reg `0x02` core enable = `0xAA`)
  3. `80 00 85 40` (reg `0x05` clock select = `0x40`)

  Round 10 had sent clock-select → clock-delay → core-enable; the
  real order is clock-delay → core-enable → clock-select.

All three are fixed values applied identically to every chip address
(not per-address-varying payloads) — only the frame's `chip_address`
byte changes, matching the existing code structure. Updated
`initialize_chip_bm1366_chain` in `thread.rs` to use these real values
and real order in place of the guessed ones, rebuilt, and — after
clearing tmpfs space on the miner (`/tmp` was 100% full from
accumulated round logs and captures; freed by removing superseded
binaries and old per-round log files) — tested against the real pool
(`pool.256foundation.org`, real npub worker ID) for the first time
with this fix.

**Result: real `Response::Nonce` decodes immediately, in volume.**
229 genuine nonces decoded from the wire in the first ~15 seconds
alone (756 by the time the run was stopped ~70s later), spanning
multiple chip addresses and job IDs, hash difficulties clustering
around the expected ~0.5 average with several individual nonces
landing far above it (2.35K, 1.40K) purely by chance — exactly the
distribution real ASIC hashing produces. At 22:40:57,
`job_source::stratum_v1` logged **"First share accepted."** — the
real pool accepted a real share, submitted with nonce `0xc21cf8ba`
against job `18cc1f2f317065d5`, from the real npub-based worker
identity. Zero panics, zero decode errors, process ran clean
throughout. This is the first confirmed instance of Mujina actually
mining Bitcoin on this S19K Pro.

One transient issue during this run, unrelated to the fix: an early
PSU voltage-ramp attempt returned "PSU returned NAK" then a checksum
mismatch, causing one chain's power-up/discovery to fail and retry —
resolved on its own (likely ordinary I2C bus noise, consistent with
occasional transient NAKs seen in earlier rounds) and didn't recur;
worth keeping an eye on but not blocking.

No lasting hardware effect: test process terminated cleanly on
SIGTERM, GPIO/PSU teardown confirmed clean (`gpio437=1`, no leftover
processes), `bosminer` restarted cleanly, fans confirmed healthy
afterward.

### Round 13: a real 15-minute soak test — stable, but caught a monitoring gap

Ran `mujina-minerd` continuously for ~16 minutes (950s) against the
real pool, the first test run longer than ~90 seconds. Discovery
succeeded on all three chains (77/77/77) on the first attempt, the
process ran the entire duration with **zero panics** and only the
same two benign discovery-phase decode warnings seen in every prior
round, and CPU/board temperatures stayed flat around 25-27°C for the
full run -- no thermal drift, no memory-growth symptoms, no
reconnects needed.

**No share landed in this run**, unlike Round 12's ~70-second test.
Root cause: the pool's vardiff was `2095` this time (queried live via
`GET /api/v0/miner`), roughly 9x higher than Round 12's `233` --
likely the pool's vardiff algorithm adjusting upward after estimating
this worker's hashrate from the prior run. At ~9x the difficulty bar,
a qualifying share is proportionally ~9x rarer for the same real
nonce-production rate, so an expected wait of several times Round
12's ~70 seconds is unsurprising and consistent with ordinary
variance, not a regression -- this needs a genuinely longer soak
(tens of minutes to hours, per the existing Next Steps item) to
confirm accept-rate behavior at realistic vardiff levels.

**Real gap found and worked around**: the established safety check
(`grep fan_rpm_feedback /etc/log/metrics/metrics.prom`) turned out to
be reading a file only `bosminer` populates. Once `bosminer` is
stopped to run `mujina-minerd`, that file's fan/temperature entries
silently stop updating -- a naive re-check returns a real, parseable,
old value with no error, which is more dangerous than an obvious
failure. Caught only because a script bug (an `awk` field-extraction
mistake) accidentally dropped the timestamp column from a monitoring
loop's output, making two consecutive readings look suspiciously
identical, which prompted a manual re-check with the timestamp
included -- confirming the reading was ~4 minutes stale. Recovered
two independent live signals instead: (1) `/sys/class/pwm/pwmchip0/
{pwm0,pwm1}/enable` and `duty_cycle` read directly (both enabled,
100% duty cycle throughout -- confirms the fans are being commanded
at full speed by hardware register state, independent of any
userspace collector), and (2) `mujina-minerd`'s own live API
(`GET /api/v0/miner` via an SSH-tunneled port, since this BusyBox
image has no `curl`/`wget`/`bash`/`python`/`nc` -- only usable from a
host with its own HTTP client) for board temperatures, which stayed
low and stable the entire run. **This board's BusyBox image has no
HTTP client at all**, discovered while trying to reach
`mujina-minerd`'s own API server from the miner itself.

No lasting hardware effect: test process terminated cleanly on
SIGTERM, GPIO/PSU teardown confirmed clean, `bosminer` restarted
cleanly, fans confirmed healthy afterward via fresh (non-stale)
telemetry.

### Round 14: pushing past 50MHz -- 300MHz confirmed real, everything above it still doesn't hash

Goal for the day: get real sustained hashrate up from Round 12/13's
~50MHz (~10.8 TH/s estimated) toward the S19K Pro's real ~100-120
TH/s range, which needs something in `bosminer`'s own proven
583.8-600.2MHz operating band.

**Voltage fixed a real bug, then turned out not to be the blocker.**
`PSU_TARGET_VOLTS` (15.2V) was correct all along -- confirmed by
directly watching a real `bosminer` cold-start: its own initial PSU
ramp target is exactly 15.2V, and searching the *entire* day's log
across every restart found no voltage value anywhere above that. But
`ramp_psu_voltage` only ever stepped *up*, never down -- watching
`bosminer`'s live fine-tuning phase (which moves voltage both
directions searching for an efficient point) made this gap obvious.
Fixed to step either direction, matching real observed behavior.

**A generic PLL formula produced real hashing at 300MHz, but nothing
higher.** `s19k-probe`'s frequency ramp (communication-only: register
write + rediscover) found all three chains clean 100-600MHz with a
50MHz-per-step jump using `protocol::Frequency::calculate_pll` --
looked like a green light. Applying that same formula to the real
mining driver at 590MHz, then 300MHz, told a different story:
**300MHz produced real accepted shares, rising temperature, and
~4-6 TH/s measured hashrate. 590MHz produced nothing** -- flat board
temperature, zero decoded nonces, for 100+ seconds of continuous job
dispatch. Communication surviving a register write says nothing
about whether the PLL reaches a working lock; only sustained
operation tests that, and the probe never did.

**Real bugs found and fixed, that still didn't solve it:**
1. Wrong fb_div range -- the formula searched BM1370's documented
   range (`0xA0-0xEF`) instead of BM1366's own (`0x90-0xEB`,
   REFERENCE.md's PLL_DIVIDER section); 590MHz's answer used
   `fb_div=0xEC`, one past BM1366's real max.
2. Coarse, single-jump-per-step ramping -- REFERENCE.md's two real
   captured bring-up traces (BM1370 and BM1362) both step
   PLL_DIVIDER in 6.25MHz increments every ~100ms, never one coarse
   jump; Round 14's ramp used 50MHz steps, 8x coarser.
3. VCO crossing mid-ramp -- a from-scratch, lowest-VCO-preferring
   search (`calculate_pll_bm1366`) still briefly touched high-VCO
   territory (flag 0x50) for some intermediate frequencies on the way
   to a low-VCO target, something neither real reference trace ever
   does.

Fixing all three still failed at 525MHz, even landing on
`fb_div=168, ref_div=2` -- the *exact* value this board's own real
50MHz capture uses, independently cross-validated by REFERENCE.md's
real BM1362 525MHz capture using the same pair. A dedicated
fixed-VCO ramp (hold `fb_div`/`ref_div` completely constant, only
ever change post-dividers) reached that exact real-cross-validated
target and still produced flat temperature, zero nonces.

**A 2-second settle delay at the final frequency** (testing whether
the PLL just needed more lock time than the 100ms per-step delay
gave it) produced an interesting but ultimately negative result: a
brief temperature rise (25.6°C -> 31.4°C) followed by a decline back
toward baseline, rather than the sustained climb real hashing
produces -- most likely a transient pulse from the ramp passing
*through* the known-working ~300MHz on its way to the target, not
real hashing at 525MHz itself.

**Binary-searching the actual failure boundary** (300MHz confirmed
working; 525MHz and 420MHz both confirmed failing) found it's much
tighter than expected: **350MHz -- the very next post-divider step
past the working 300MHz -- already fails.** Noticed a pattern across
every config tried: both real working points (this board's captured
50MHz, and 300MHz) use `post_div1=7`; every failing config
(350/420/525MHz) used a smaller `post_div1` (6/5/4). Testing the
hypothesis directly -- hold `post_div1=7, post_div2=1` fixed and
raise `fb_div` instead, landing on 350MHz via `fb_div=196` rather
than a smaller `post_div1` -- also failed (flat temperature, zero
nonces after 110+ seconds). Ruled out.

**A real capture attempt at the real target frequency hit a wall.**
Tried extending `s19k-trace` (the original ptrace tracer) with a
seccomp-bpf filter that traps only the `write` syscall, so every
other syscall an async runtime makes (thousands/sec) runs untouched
-- CPU overhead dropped from ~70% to ~3%, and `bosminer` no longer
hit its own hashchain-init timeout the way it did under the original
tool's full-syscall tracing. But even after fixing a real
seccomp+ptrace ordering bug (the filter must not go live until the
tracer has `PTRACE_O_TRACESECCOMP` armed, or early-trapped syscalls
get silently converted to ENOSYS instead of a clean stop -- confirmed
by watching an unconditional-trace filter kill `bosminer` instantly
before the fix, and stop crashing after it), **the filter still never
produces a single seccomp stop**, even with an unconditional
trace-everything filter. `/proc/<pid>/status` confirms the filter
installs (`Seccomp: 2`), but nothing ever traps. Root cause not
found -- possibly a real quirk of this board's old (4.9.113), heavily
vendor-customized aarch64 kernel running these chips' driver as a
32-bit ARM EABI compat process. Shelved rather than sunk further time
into it. `s19k_trace_fast.rs` was later removed from the tree rather
than shipped in a known-non-working state; it is preserved in git
history at commit `398072d` for whoever picks this up next.

**Net result**: six real, well-reasoned, individually-tested
hypotheses (formula-computed PLL choice at two different search
strategies, ramp granularity, VCO stability during the ramp, settle
time, and post-divider preservation) all ruled out. **300MHz is
solid, real, repeatedly-confirmed working hashing** -- the exact
same signals (rising temperature, real hashrate, real accepted
shares) every single higher frequency tried today failed to produce,
with zero exceptions. Whatever's different about this board's real
583.8-600.2MHz operation remains genuinely unknown; getting real
wire-level visibility into it (the one approach not yet successful)
is the most likely way to actually resolve this, not more guessing
at register encodings.

No lasting hardware effect across the entire day's testing:
`bosminer` restarted cleanly multiple times, fans/temps confirmed
healthy after every test, no leftover processes.

### Round 14, continued: a real external reference, three real fixes applied together, still no hashing above 300MHz

Partway through Round 14 the user surfaced a real, independently-
developed reference for this exact problem:
**github.com/Schnitzel/mujina, `amlogic-s19kpro-support` branch** --
a fork of this same project with its own from-scratch BM1366/S19K
Pro (BHB56902 hashboard) support, including its own real hardware
testing (**39.68 TH/s measured at 575MHz on their own mujina fork**,
matching LuxOS's 39.15-39.33 TH/s at the same point on the same
chips). This is a real, working reference for the exact chip family
and exact hashboard model this project targets -- genuinely
different in kind from anything tried earlier today.

That reference's code (`mujina-miner/src/asic/bm13xx/chip_config.rs`,
`sequencer.rs`) revealed three concrete, sourced differences from
this project's approach:

1. **BM1366 requires strict `post_div1 > post_div2`**, not `>=`.
   Sourced from bitaxeorg/ESP-Miner's real firmware
   (`components/asic/bm1366.c`'s `pll_get_parameters(target, 144,
   235, ...)`), not a guess. Every PLL search this project tried all
   day (including the from-scratch `calculate_pll_bm1366`) used
   `>=`, which allows `post_div1 == post_div2` -- confirmed as a real
   bug: the very first non-startup step in every fixed-VCO ramp tried
   today (product 36 = 6x6) violated this constraint outright.
2. **Real factory operating voltage is 13.9V**, not 15.2V. Sourced
   directly from Braiins' own bosminer log on this exact hashboard
   model (`Detected hashboard #2: Voltage (Avg.) 13.90 V, Frequency
   (Avg.) 645 MHz`). Every Round 14 test today ran the entire time at
   the 15.2V bring-up ceiling -- correct as a *startup* value (this
   project's own real `bosminer` observation confirms 15.2V is the
   real initial ramp target) but never adjusted down before hashing.
3. **The frequency ramp runs last**, after TicketMask, NonceRange,
   and the baud switch -- not before them, which is what every Round
   12-14 sequence (including this project's own) has always done.

`PSU_TARGET_VOLTS` was corrected to 13.9V (`board/
antminer_s19k_am3.rs`), `calculate_pll_bm1366` was fixed to enforce
strict `post_div1 > post_div2`, and the per-chain init sequence was
reordered to match (TicketMask/NonceRange/baud-switch now run before
the frequency ramp, matching the reference exactly). Verified in the
real wire log that all three changes took effect correctly (every
sent post-divider pair satisfies the strict inequality; the PSU
ramps to 13.9V; the sequence order matches). Targeted 575MHz -- the
reference's own real measured operating point, deliberately not the
645MHz factory ceiling their own notes call "right at the edge of
stability."

**Still failed** -- flat board temperature, zero decoded nonces,
after 130+ seconds of continuous job dispatch. Same failure
signature as every other frequency tried today. This means either a
fourth, still-unidentified real difference exists, or something more
subtle in how these three fixes were adapted/ported doesn't fully
match the reference's real behavior.

**Not yet tried, and the highest-value next step**: actually build
and run the reference project's own binary directly against this
board, rather than continuing to manually port pieces of its logic.
If their proven code also fails on this specific physical hardware,
that points to something board-specific (this unit's chips, or
something about today's ~3+ hours of repeated power-cycling/testing)
rather than a porting gap. If it works, that's decisive proof the
gap is specifically in how their logic was adapted here, and the
fastest path forward is likely porting substantially more of their
`sequencer.rs`/`chip_config.rs`/`thread_v2.rs` wholesale rather than
hand-translating individual pieces. Their repo was cloned to
`~/mujina-s19kpro-support-ref` (or wherever it lands next session --
not currently checked into this project) for exactly this purpose;
building it needs its own `mujina.toml`/`mujina-hb2.toml`-style
config adapted for this board's real GPIO/tty mapping (chain 1-3 to
ttyS1-3 via reset_gpio 454-456, confirmed identical hardware to what
their own `mujina.toml` example already documents for the sibling
S19j Pro board on the same Amlogic A113D control board family) --
started but not completed this session.

No lasting hardware effect: every test in this continued
investigation ended in a clean stop, safe GPIO/PSU teardown, and a
clean `bosminer` restart with healthy fans/temps confirmed
afterward.

### Round 15: SOLVED -- a byte-order bug on the BM1366 `Core` register was disabling almost every hashing core

**Result: ~85 TH/s sustained at 575MHz with real accepted shares on
`pool.256foundation.org`, up from ~2.9 TH/s.** The ~300MHz ceiling
and the "zero nonces above 300MHz" symptom were both downstream of a
single wrong value.

#### The bug

`Register::Core` (`0x3C`, `CORE_MAILBOX`) is encoded **big-endian**
in `protocol.rs`'s `encode_data` (`dst.put_u32(...)`), while every
other raw register is encoded little-endian (`dst.put_u32_le(...)`).
That asymmetry is real and deliberate -- it matches the captured
wire bytes and is covered by a capture-backed test.

The Schnitzel reference's `chip_config.rs` stored BM1366's `Core`
values in *little-endian byte order*:

```rust
core_broadcast: [0x4085_0080, 0x2080_0080],
core_perchip:   [0x4085_0080, 0x2080_0080, 0xAA82_0080],
```

Big-endian-encoded, `0x4085_0080` goes onto the wire as
`40 85 00 80` -- the exact byte-reverse of the intended
`80 00 85 40`. `bm1362()` and `bm1370()` store the same registers in
natural big-endian order (`0x8000_8540`, ...) and were unaffected;
only BM1366 was wrong, which is why this survived: the S21 Pro and
S19j Pro paths were fine.

**Why it costs almost all the hashrate:** `PROTOCOL.md` documents
bit 31 of `CORE_MAILBOX` as always set -- it is the "apply to all
cores" broadcast bit. In the reversed bytes bit 31 is **clear**, so
every core write (clock delay, clock select, and critically
**core-enable `0xAA`**) was addressed to a single garbage `core_id`
instead of the whole chip. Chips still enumerated 77/77, still
accepted every register write, still ramped their PLL, and still
reported *some* nonces -- from whichever cores happened to be
enabled by default -- which is exactly why every previous round's
diagnostics looked healthy.

The fix is to store them in the order the encoder expects:

```rust
core_broadcast: [0x8000_8540, 0x8000_8020],
core_perchip:   [0x8000_8540, 0x8000_8020, 0x8000_82AA],
```

**The guarding unit test was asserting the wrong encoder.**
`bm1366_init_regs_match_esp_miner_wire_bytes` checked
`raw.to_le_bytes()` for *all* registers including `Core`, so the
byte-reversed values passed CI while the wire got the reverse. The
test now selects endianness per register (big-endian for `Core`,
little-endian for the rest), matching `encode_data`. Any future work
adding a register needs its endianness checked against its own
`encode_data` arm -- this is the second time this exact asymmetry has
caused a real bug (Round 9 hit it on the *decode* side).

#### Verified result

Measured by counting reported nonces (each one represents 2^34
hashes at the confirmed `TicketMask` `zero_bits=2`, so
`TH/s = nonces_per_sec * 0.01718`):

| Metric | Before fix | After fix |
|---|---|---|
| Reported nonce rate | ~170/s | **~4,950/s** |
| Implied hashrate | ~2.9 TH/s | **~85 TH/s** |
| Pool 5-min rate (pool's own API) | 0 | **21.7 TH/s and climbing** |
| Accepted shares | 0 | **29 of 57 in first window** |
| Pool vardiff | 8,927 | **71,211** |
| Best difficulty seen | ~995 | **2,553,937** |
| Max board temp | 75C (tripped overtemp) | **46.4C** |

`bosminer` on this same unit measures **104 TH/s** (1.88s/share at
difficulty 45,659, cross-checked: `45659 * 2^32 / 1.04e14 = 1.885s`
-- internally consistent), so ~85 TH/s is in the right league and
well past the reference project's own 39.68 TH/s claim.

**A thermal cross-check worth recording**, because it independently
confirms the mechanism: before the fix, ~2 TH/s with fans at 50%
reached 75C and tripped the overtemp cutoff. After the fix, ~85 TH/s
with fans at 100% sits at 46C. A 40x hashrate increase cannot
produce *less* heat -- which means the chips were already drawing
roughly this much power beforehand, switching at 575MHz but doing
almost no useful work. Efficiency went from order ~1,000 J/TH to
~23 J/TH (nameplate is 120 TH/s @ 2760W = 23.0 J/TH). Landing at
nameplate efficiency is itself evidence the fix is complete rather
than partial.

Estimated power draw at this operating point is **~2,000W**
(1,850-2,250W); there is **no way to measure it from software** --
the APW12 protocol as implemented exposes only voltage setpoint
(`0x03`), measured voltage (`0x04`), and on/off state (`0x05`). No
current or power command exists. A wall meter or PDU reading is the
only route to a real number.

#### How the reference binary was made to run on this unit

Getting the Schnitzel fork onto this hardware needed three things
beyond a plain build:

1. **A sibling dependency on an unmerged branch.**
   `mujina-miner/Cargo.toml` has `amlogic-cb-tools = { path =
   "../../amlogic-cb-tools" }`. `github.com/Schnitzel/amlogic-cb-tools`
   `main` lacks the `pic` module the board driver imports; it lives on
   the **`pic-microcontroller-driver`** branch. Clone that branch as a
   sibling directory or the build fails on `unresolved import
   amlogic_cb_tools::pic`.
2. **A bit-bang I2C PSU shim.** The reference talks to the PSU via
   `LinuxI2cDevice::open(config.psu.i2c_device, ...)`, i.e. a real
   `/dev/i2c-N` chardev. On this unit that fails with ENXIO -- the PSU
   is only reachable over bit-banged GPIO 476/477 (`I2C_SCL`/`I2C_SDA`
   in Bitmain's own `/etc/init.d/S37board_setup`), exactly as this
   project's own driver already knew. A synchronous bit-bang shim
   (`board/psu_bitbang_i2c.rs`, same START/STOP/ACK logic as this
   project's `linux_hw/bitbang_i2c.rs` but blocking, exposing
   `write_byte_transaction`/`read_byte_transaction`) dropped into
   `NativeAmlogicPsu::exchange` in place of `LinuxI2cDevice` is
   sufficient -- the PSU framing itself
   (`build_frame`/`parse_frame`/`CMD_*`) is unchanged and correct.
3. **`default_fan_percent = 100`.** The reference has **no dynamic
   fan control at all** -- `configure_fans(config,
   config.startup.default_fan_percent)` is set once at startup and
   never revisited. The example configs ship `50`, which caps fans
   around 4,200-5,400 RPM where `bosminer` reaches ~7,000-7,900 RPM.
   At 50% the board hit the overtemp cutoff in ~4 minutes. This is a
   real gap versus stock firmware and must be raised for any
   sustained run.

Runtime config: frequency is **not** configurable at runtime --
`target_frequency_mhz: Some(575.0)` in `chip_config.rs`'s `bm1366()`
is compile-time only. There is no frequency/voltage field in
`mujina.toml`, no env var, and no API route (the v0 API's only
mutation is `PATCH /miner` with `paused`). Changing frequency means
editing `chip_config.rs` and recompiling.

#### HashScope as the measurement harness

`github.com/256foundation/HashScope` was stood up in the dev
container as a transparent Stratum MITM proxy and is what produced
the authoritative before/after numbers. `docker compose up -d
backend` (the frontend is optional -- the REST API is enough), with
`POOL_HOST=stratum+tcp://pool.256foundation.org` / `POOL_PORT=3333`
in `.env`; point the miner at `stratum+tcp://<dev-container>:3333`.
Useful endpoints: `GET /api/sessions` (per-session assigned
difficulty, message counts, user agent -- `bosminer` and `mujina`
appear as separate sessions and can be compared directly) and
`GET /api/messages?session_id=...` (fully decoded `mining.submit` /
`mining.notify` with paired responses and latency, which is how
accept-vs-reject was counted).

Note the dev container needed `usermod -aG docker` and commands run
via `sg docker -c '...'` since the shell predated the group change.

#### Where the working code lives

The Round 15 work was done in a session scratchpad, which is
ephemeral. It has been copied to a durable location:

```
~/mujina-s19kpro-ref/
  schnitzel-mujina/     # branch amlogic-s19kpro-support @ 9837ef7, WITH the fixes applied
  amlogic-cb-tools/     # branch pic-microcontroller-driver @ df2c1b8 (sibling path dep)
  HashScope/            # MITM stratum proxy used for measurement
  s19k-fixes.patch      # self-contained diff of every change made (5 files)
```

`target/` was excluded, so the first build there will be a full
rebuild. To reproduce from scratch instead:

```sh
git clone --branch amlogic-s19kpro-support --single-branch \
  https://github.com/Schnitzel/mujina.git schnitzel-mujina
git clone --branch pic-microcontroller-driver --single-branch \
  https://github.com/Schnitzel/amlogic-cb-tools.git amlogic-cb-tools
git clone https://github.com/256foundation/HashScope.git
cd schnitzel-mujina && git apply ../s19k-fixes.patch
```

The patch contains: the `Core` byte-order fix + its corrected unit
test (`chip_config.rs`), the `NonceRange` revert (`sequencer.rs`),
and the bit-bang PSU shim (`board/psu_bitbang_i2c.rs` +
`board/mod.rs` + `board/s19k_pro_amlogic.rs`).

Build for this board with the same recipe as this project (see
[Toolchain & build recipe](#toolchain--build-recipe)):
`cargo build --release --target armv7-unknown-linux-musleabihf --bin
mujina-minerd`. Note `cargo test` does **not** work on the x86_64
host — the `udev` dependency needs `libudev` dev headers that aren't
installed; only the musl cross-target excludes it.

The board config used (`mujina-s19k-real.toml`, deployed to
`/tmp/mujina-s19k-real.toml` on the miner) mirrors the reference's
own `mujina.toml` example with this unit's real mapping: chains at
index 0/1/2 → `/dev/ttyS1`/`ttyS2`/`ttyS3`, `reset_gpio` 454/455/456,
`detect_gpio` 439/440/441, `temp_i2c_device`/`eeprom_i2c_device`
`/dev/i2c-1`, PSU `enable_gpio` 437, and
**`default_fan_percent = 100`**.

#### Two regressions introduced and reverted during this round

Recorded because both looked plausible and both were wrong:

1. **`NonceRange` "partitioning".** Replacing the broadcast
   `NonceRangeConfig::from_raw(chip_config.nonce_range)` with
   `NonceRangeConfig::multi_chip(chain.chip_count())` on the theory
   that a single broadcast value makes every chip search the same
   subrange. Wrong: `multi_chip()` buckets 77 chips into its
   `65..=128` arm and sends the **S21 Pro** value (`00 00 1e b5`),
   discarding the BHB56902 LuxOS-captured BM1366 value
   (`00 00 10 5a`), and real firmware does broadcast a single value
   here while still reaching ~39 TH/s. Reverted. Note this also
   left `chip_config.nonce_range` dead for a while -- worth grepping
   for orphaned config fields after changes like this.
2. **A suggested-difficulty floor.** Forcing
   `compute_suggested_difficulty` up to `bosminer`'s observed 45,659
   made the pool assign 125,000, which made qualifying shares ~14x
   rarer and *hid* evidence rather than producing it. Reverted.

#### Measurement lessons from this round

Both cost real time and are worth not repeating:

- **`Difficulty`'s `Display` uses SI suffixes.** Values >=1000 print
  as `1.5K`, `112K`, `1.2M`. A regex of `hash_diff=[0-9.]+` silently
  captures the mantissa and drops the suffix, turning 45,600 into
  "45.6". This produced a bogus "max difficulty is capped near 1000"
  finding that two separate false theories were then built on. Match
  `hash_diff=[0-9.]+[KMGTP]?` and scale.
- **`shares_submitted` and per-thread `hashrate` from the API are
  not measurements.** Per-thread hashrate is the fixed nominal
  constant `83 GH/s * chip_count` (6.39 TH/s per board, 19.2 TH/s
  for three), set once in the constructor and never updated from
  real nonces. It reads identically whether the miner is doing
  2 TH/s or 85 TH/s. Only the scheduler's `HashrateEstimator`, the
  nonce-rate arithmetic above, or the pool's own API are real.
- **Log filters must be checked against the emitting module's
  level.** Several "the code path never fires!" conclusions were
  just the wrong `RUST_LOG` target: `handle_share`'s stale-task
  early-return is `trace!`, and the scheduler's `"Share found"` is
  `debug!` under `mujina_miner::scheduler`, not under
  `asic::bm13xx`.

#### Still open after this round

- ~~**~49% share reject rate.**~~ **RESOLVED -- transient vardiff
  artifact, not a defect.** The initial 29-accepted/28-rejected
  window was measured while the pool was still ratcheting difficulty
  up (8,927 -> 71,211 -> 166,909) in response to the newly-real
  hashrate. Shares computed against the older, lower target arrive
  after the pool has moved, and get a bare
  `{"result": false, "error": null}`. Splitting the same session's
  199 submits chronologically shows it clearly: **32.3% reject in the
  earlier half, 0.0% in the later half** once vardiff settled.
  Submit latency is healthy throughout (39-47ms, avg 41ms --
  comparable to `bosminer`'s ~38ms on the same proxy). No fix
  needed; expect a burst of rejects after any hashrate step change.
- **Whether this project's own driver has the same bug.** All of
  Round 15's work was against the *reference* binary. This project's
  own `board/antminer_s19k_am3.rs` + `asic/bm13xx/thread.rs` path
  has not been retested since the fix was found. Its `Core` values
  come from this board's own captured table rather than
  `chip_config.rs`, so it may already be correct -- but Round 9
  already found a `Core` endianness bug on the decode side here, so
  the encode side needs the same check. **This is the highest-value
  next step for this project specifically**, since Round 15 proves
  the hardware reaches ~85 TH/s.
- **~25 of 231 chips lost during the frequency ramp** (`Chain
  verification failed: not all chips responded`, 68/77 and 61/77 on
  two boards, 77/77 on the third). Real but secondary at ~89% of
  chips working.
- **No PLL readback still exists** in either codebase, so "did the
  chips actually reach the commanded frequency" remains unverifiable
  by direct measurement. It stopped being the leading hypothesis once
  the `Core` fix produced nameplate-efficiency hashrate, but the gap
  is still there. Register `0x08` has never been read back on this
  hardware; LuxOS reportedly does exactly this (`4205 08` reads
  between PLL writes) and the reference explicitly notes it does not.

## Next steps

The ~300MHz ceiling is **solved** — see
[Round 15](#round-15-solved--a-byte-order-bug-on-the-bm1366-core-register-was-disabling-almost-every-hashing-core).
This hardware now runs at **~85 TH/s at 575MHz with real accepted
shares**, at nameplate efficiency (~23 J/TH), verified against the
pool's own API. That was achieved with the **Schnitzel reference
binary** plus a bit-bang PSU shim, not with this project's own
driver.

1. **Port the `Core` byte-order fix into this project's own driver
   and retest.** Round 15's fix was verified against the reference
   binary. This project's `board/antminer_s19k_am3.rs` +
   `asic/bm13xx/thread.rs` path replays its own captured `Core`
   table, so it may already emit correct bytes — but Round 9 found a
   `Core` endianness bug on the *decode* side here, so the encode
   side needs the same audit. Check what the per-chip `Core` triplet
   actually puts on the wire (must be `80 00 80 20`,
   `80 00 82 AA`, `80 00 85 40` — bit 31 **set**), then retest at
   575MHz. Round 15 proves the hardware gets there, so anything less
   from this driver is now a driver bug, not a hardware limit.
2. **Run a real multi-hour soak at 575MHz.** The first ~20 minutes
   are clean (105.6 TH/s pool-side, 0% reject at settled vardiff,
   49C), but longer-run questions are untested: thermal drift,
   memory growth, pool reconnect handling, and whether chip count
   degrades over hours. Note the reference has **no dynamic fan
   control**, so `default_fan_percent` must be 100 for any sustained
   run; at 50% it tripped overtemp in ~4 minutes.
3. **Try pushing past 575MHz.** The reference caps `max_freq` at
   660MHz and the EEPROM ATE setpoint is 645MHz; `bosminer` operates
   at 583-600MHz and reports 645MHz per-board. Now that cores are
   actually enabled, the earlier conclusion that ">300MHz doesn't
   work" is void and the real frequency/voltage headroom is
   unexplored. `target_frequency_mhz` in `chip_config.rs`'s
   `bm1366()` is compile-time only, so each point needs a rebuild.
4. **Fix the fan-safety monitoring gap Round 13 found**: the
   established check (`grep fan_rpm_feedback
   /etc/log/metrics/metrics.prom`) only gets fresh data while
   `bosminer` is running — it goes silently stale (no error, just an
   old-but-valid-looking reading) the moment `bosminer` is stopped to
   run `mujina-minerd`. Worked around this whole session by reading
   `/sys/class/pwm/pwmchip0/{pwm0,pwm1}/enable`+`duty_cycle` directly
   and `mujina-minerd`'s own `/api/v0/miner` endpoint (temperatures
   only — it doesn't report fan RPM at all, `"fans": []` in every
   response so far). A real fix would be either wiring real fan RPM
   into `mujina-minerd`'s own telemetry, or documenting the PWM-sysfs
   + API combination as the standard check whenever `bosminer` isn't
   the one running. **Always check the timestamp, not just the
   value** — a stale reading looks identical to a fresh one otherwise.
5. **Investigate the transient PSU "NAK"/checksum-mismatch retries**
   seen during Round 12/13/14 startups. Always self-resolved so far,
   but worth understanding root cause before trusting this for
   unattended operation.
6. **Decode what the per-chip domain config writes actually do**,
   rather than replaying them verbatim — Round 5's 32 captured
   `IoDriverStrength`(`0x58`)/`UartRelay`(`0x2C`) writes work, but
   only cover ~every 7th chip address directly. Understanding the
   real semantics (rather than cargo-culting captured bytes) would
   make the driver more maintainable and less fragile to firmware
   version differences.
7. **Correct the BM1362 → BM1366 assumption throughout the codebase
   and docs.** Round 5 found the real chips self-report as BM1366.
   `s19k_probe.rs`'s header comment and any other "BM1362" references
   inherited from the S19j Pro reference capture should be updated —
   the wire *frame format* matches closely enough that byte-level
   encoding was never the issue. `board/antminer_s19k_am3.rs`'s own
   module docs already reflect BM1366.
8. **Once a real soak test confirms stability** (frequency itself is
   solved as of Round 15), consider replacing `bosminer` as the
   running daemon
   (swap what `/etc/init.d/S99bosminer` launches). Keep `bosminer`
   available as a fallback until `mujina-minerd` has proven itself
   reliable over a real extended run at a real target frequency.

## Prior art referenced

- **[skot/amlogic-cb-tools](https://github.com/skot/amlogic-cb-tools)**
  — a standalone Rust diagnostic toolkit (not part of Mujina)
  targeting this Amlogic A113D board family, kept deliberately
  separate "to enable independent hardware testing before broader
  integration." Its GPIO map and I2C addresses cross-validated
  cleanly against this project's own independent findings. Its
  `apw12-psu-tool` documents the correct APW12 protocol shape but its
  own transport implementation doesn't reach the PSU on this specific
  board (see the PSU section above) and its `scan` command caused the
  incident above — read any of its code before running it against a
  live address. Its `controlboard-misc` sub-tool's GPIO labels
  (green/red LED, buttons) independently corroborate
  `S37board_setup`.
- **[HashSource/Antminer-APW12-Firmware](https://github.com/HashSource/Antminer-APW12-Firmware)**
  — real APW12 PSU firmware dumps and disassembly notes, used here
  to independently confirm the PSU's I2C address from its actual
  PIC16F1704 firmware.

## Process notes

- Full flash-partition backups (`mtd1`-`mtd5`, ~250MB) and BraiinsOS
  config files were pulled during the original recon pass — they
  live locally on the machine used for that session's scratchpad (not
  checked into this repo; regeneratable via the same `dd`-over-SSH
  approach documented in the recon report if ever needed for
  restore).
- The `mujina` clone with all patches and new probe binaries lives at
  `~/mujina` on the `<dev-host-ip>` container, as local git commits
  (not pushed anywhere yet — no fork has been created for this work
  specifically).
