# Antminer S19K Pro (AM3/Amlogic control board) — hardware reference

Target unit: `ssh root@<miner-ip>`, stock BraiinsOS+ (build
`2026-07-07-0-c5a2978a-26.07-plus`), owned by the user and explicitly
accepted as a sunk cost for this work. Root SSH is open by default
(BraiinsOS ships this way — no exploit involved).

This is the current, corrected technical reference. It supersedes the
architecture assumptions in the original recon pass (2026-08-24) —
see [s19k-pro-recon-report.md](reference/recon-methodology.md) for that
session's methodology, and the "Revision history" section at the
bottom of this document for exactly what changed and why.

## Machine identity

- Model: Antminer S19K Pro, "NoPic" variant (no per-board PIC
  controller)
- Performance profile: 120T, 2760W
- 3× **BHB56902** hashboards, PCB rev `0x0003`, each with **77
  BM1366** chips (231 total). Note: earlier revisions of this
  document said BM1362, inherited from the S19j Pro reference
  capture. The chips self-report as **BM1366** over the wire — see
  the bring-up log's Round 5. The wire *frame format* is close
  enough between the two that byte-level framing was never the
  issue, but the BM1366's PLL constraints and `Core` register
  values genuinely differ.
- PSU: **APW121215d/e**, protocol/hardware version `0x75`, firmware
  version `0x0019` (read live via the real APW12 protocol — see
  below)

## Control board

- SoC: **Amlogic A113D** ("AXG" family) — quad-core Cortex-A53
- `bos_platform` = `am3-aml`
- Buildroot 2019.02.7, kernel 4.9.113

### Architecture — read this before assuming anything about the target triple

`uname -m` reports `aarch64`, and it's easy to stop there. **Don't.**
The kernel is 64-bit, but the actual userspace is **32-bit ARM
hard-float** (EABI5). Confirmed directly:

```
$ readelf -h bosminer
  Class:    ELF32
  Machine:  ARM
  Flags:    0x5000400, Version5 EABI, hard-float ABI
```

`bosminer` and `busybox` are both **fully static** ARM32 executables
— no dynamic section, no ELF interpreter at all. This is a Buildroot
userland, not glibc/musl-dynamic. The correct Rust target for
anything meant to run natively on this board is
**`armv7-unknown-linux-musleabihf`**, statically linked, matching
`bosminer`'s own build exactly.

The 64-bit kernel *can* still execute genuine aarch64 ELF64 binaries
(confirmed: skot's prebuilt `apw12-psu-tool`, built for
`aarch64-unknown-linux-musl`, runs fine) via the kernel's standard
32/64-bit compat support. That a 64-bit binary happens to run is not
evidence the userspace itself is 64-bit — it very much isn't.

See the [build recipe in the engineering log](reference/full-engineering-log.md#toolchain--build-recipe)
for the exact cross-compilation setup this implies.

## UART chains (hashboard communication)

| Device | Role | Confirmed via |
|---|---|---|
| `ttyS0` | Console at boot; idle chatter every ~60s | boot log |
| `ttyS1` | A hashboard chain's data port | boot log, `dmesg` (which specific chain is unresolved — see below) |
| `ttyS2` | A hashboard chain's data port | boot log, `dmesg` (which specific chain is unresolved — see below) |
| `ttyS3` | A hashboard chain's data port | boot log, `dmesg` (which specific chain is unresolved — see below) |

Per skot's independently-derived board notes (`/etc/init.d/
S37board_setup` doesn't label the UARTs directly, only GPIO — see the
[GPIO map](#gpio-map-periphs-banks-controller-base-411-86-lines)
below for that source):

- HB0 UART: `ttyS3` (`GPIOAO_4`/`GPIOAO_5`, `uart_ao_b`)
- HB1 UART: `ttyS2` (`GPIOZ_2`/`GPIOZ_3`, `uart_b`)
- HB2 UART: `ttyS1` (`GPIOX_8`/`GPIOX_9`, `uart_a`)

**This is unresolved and genuinely in question, not just a
notational quirk.** If HB0/HB1/HB2 above corresponds 1:1 to
chain 1/2/3 (the natural reading), it conflicts with the naive
chain-N→`ttyS`N mapping used by `s19k_probe.rs` and implied by the
table above. An empirical cross-matching test (chain 1's enable GPIO
against all three `ttyS` devices) didn't resolve it either way —
nothing responded on *any* UART regardless of which one was tried,
for reasons unrelated to this numbering question (see
[the engineering log's chip-discovery investigation](reference/full-engineering-log.md#chip-discovery-still-silent-several-hypotheses-now-ruled-out)).
Don't assume either mapping is correct until something actually
responds to test against.

**Baud rate: 3,125,000**, confirmed multiple ways — `bosminer`'s own
log (`CHAIN/1: Set baud rate @ requested: 3125000, actual: 3125000`)
and kernel driver messages (`meson_uart ffd24000.serial: ttyS1 ...
change 3000000 to 3125000` — the brief stop at 3,000,000 is a
`meson_uart` driver rounding artifact of the initial open, not a
deliberate low-speed negotiation step; a fresh open at exactly
3,125,000 works directly). An earlier session log describing a
"115200 → 9600 → 115200 → 3,125,000" ramp could not be reproduced
against a clean, per-device `dmesg` capture and was most likely a
misreading of interleaved log lines from multiple UARTs — trust the
per-device `dmesg` evidence over that description if they ever
conflict.

Each chain enumerates 77 chips:
`CHAIN/n: Discovered 77 chips (expected 77 chips)`.

Passive listening on all 4 UARTs with `bosminer` stopped yields
**zero unsolicited bytes** — the chips and PSU are pure slaves,
silent until polled. A driver needs to actively speak the relevant
protocol (BM13xx for the chips, APW12 for the PSU) to get anything
out of them.

## GPIO map (periphs-banks controller, base 411, 86 lines)

The definitive source for this table is
**`/etc/init.d/S37board_setup`** — Bitmain/Braiins' own boot script,
plain shell, fully readable on the live system (`cat
/etc/init.d/S37board_setup`). It exports and labels every GPIO line
the stock firmware uses, in comments, straight from the vendor. This
is first-party ground truth, not inference — prefer it over anything
below marked "inferred" or "per skot's tool."

| Line(s) | Vendor label (from `S37board_setup`) | Direction | Notes |
|---|---|---|---|
| `437` | `PSU_nEN` | out | **Active-low PSU enable.** `0` = PSU enabled, `1` = disabled. Confirmed both by the vendor's own naming and by observed behavior (`0,0`-ish while mining, `1` when `bosminer` is stopped). |
| `438` | `GPIO_LED_RED` | out | Red status LED. |
| `453` | `GPIO_LED_GREEN` | out | Green status LED. Blinks during normal mining — this is why earlier passive observation mistook it for a communication bus; it isn't one. |
| `476` | `I2C_SCL` | dynamic | **Bit-banged PSU I2C bus, clock.** See "PSU communication" below — this is the actual answer to where PSU comms live. Exported as `in` at boot (pinmux-only, per the script's own comment: "prevent I2C bus corruption when accessing PWM"); driven as `out` by `bosminer` itself during real transactions. |
| `477` | `I2C_SDA` | dynamic | **Bit-banged PSU I2C bus, data.** Same bus as 476. |
| `445` | `GPIO_IP_GET` | in, rising-edge | "IP report" button. |
| `446` | `GPIO_RECOVERY` | in, rising-edge | Recovery button. |
| `439` | `CH0_PLUG` | in, pulled down | Chain 1 hashboard presence-detect. |
| `440` | `CH1_PLUG` | in, pulled down | Chain 2 hashboard presence-detect. |
| `441` | `CH2_PLUG` | in, pulled down | Chain 3 hashboard presence-detect. |
| `454` | `CH0_RST` | out | Chain 1 hashboard enable/reset. `1` = enabled. |
| `455` | `CH1_RST` | out | Chain 2 hashboard enable/reset. `1` = enabled. |
| `456` | `CH2_RST` | out | Chain 3 hashboard enable/reset. `1` = enabled. |
| `447` | `FAN_FRONT_SPEED0` | in, falling-edge | Fan tachometer. |
| `448` | `FAN_FRONT_SPEED1` | in, falling-edge | Fan tachometer. |
| `449` | `FAN_REAR_SPEED0` | in, falling-edge | Fan tachometer. |
| `450` | `FAN_REAR_SPEED1` | in, falling-edge | Fan tachometer. |

The `454`/`455`/`456` chain-enable naming ("_RST" in the vendor
script) and the "hashboard enable" description used elsewhere in
this document refer to the same three lines — the original recon
independently derived their function (watching `bosminer`'s own
graceful stop/start move them `1`→`0`) before this vendor source was
found, and both agree.

`gpiochip497` (aobus-banks controller, base 497, 15 lines): no lines
exported by current firmware.

## Fans

Hardware PWM, separate from the tachometer GPIOs above:

- `/sys/class/pwm/pwmchip0/pwm0` — rear fans (FAN2/FAN4)
- `/sys/class/pwm/pwmchip0/pwm1` — front fans (FAN1/FAN3)
- 100 kHz period (`100000` ns), per `S37board_setup`

## I2C

Two hardware I2C controllers are active at runtime
(`/proc/iomem` confirms only these two of five controllers defined
in the devicetree are actually memory-mapped):

| Device node | Underlying controller | What's on it |
|---|---|---|
| `/dev/i2c-0` | cbus `i2c@1e000` | Nothing populated — confirmed via `i2cdetect`, completely silent. |
| `/dev/i2c-1` | aobus `i2c@5000` (`i2c_ao`, AO-domain) | TMP75 temperature sensors + per-board EEPROMs (see below). |

**TMP75 temperature sensor addresses** (on `/dev/i2c-1`):

| Hashboard | Inlet | Outlet |
|---|---|---|
| HB0 (chain 1) | `0x48` | `0x4C` |
| HB1 (chain 2) | `0x4D` | `0x49` |
| HB2 (chain 3) | `0x4E` | `0x4A` |

**EEPROM addresses** (on `/dev/i2c-1`, one per hashboard, real
24Cxx-style writable parts storing board identity/calibration data
`bosminer` parses at startup):

| Hashboard | Address |
|---|---|
| HB0 (chain 1) | `0x50` |
| HB1 (chain 2) | `0x51` |
| HB2 (chain 3) | `0x52` |

**These are live, writable parts that real board-identification data
depends on.** See the "EEPROM corruption incident" note in
the engineering log before running any third-party I2C tool against these
addresses that you haven't personally read the source of — a `scan`
command that looks read-only can still perform real register writes.

Three more I2C controllers exist in silicon
(`soc/cbus@ffd00000/i2c@{1c,1d,1f}000`) but are `status = "disabled"`
in the devicetree and never get memory-mapped. No devicetree-overlay
support exists on this kernel
(`/sys/kernel/config/device-tree/overlays` doesn't exist) and no
loadable `i2c-gpio` module is present (`lsmod` is empty — this
kernel has no loadable-module support at all; everything is built
in). Reaching these dormant controllers would require a devicetree
edit plus reboot — not attempted, and a materially different risk
category than anything else in this document (see the engineering log's risk
notes).

### PSU communication — this is *not* on either hardware I2C bus

This took real effort to nail down, so it's worth stating plainly:
**the PSU is not reachable via `/dev/i2c-0` or `/dev/i2c-1` at all.**
5 minutes of kernel `i2c` ftrace tracing (every `i2c_write`/`i2c_read`
/`i2c_reply`/`i2c_result` event, spanning a full cold boot through
steady-state mining) shows zero traffic to any address other than
the TMP75/EEPROM ones above. `bosminer`'s only open I2C file
descriptor for its whole lifetime is `/dev/i2c-1` — no PSU-dedicated
`/dev/i2c-N` node is ever opened.

The actual PSU bus is **software bit-banged over GPIO 476 (`I2C_SCL`)
and GPIO 477 (`I2C_SDA`)**, per `S37board_setup`'s own labels (see
the GPIO table above). Bit-banged GPIO writes never pass through the
kernel's `i2c_transfer()`, so the standard `i2c` ftrace tracepoints
can never see them — this is a bus that is architecturally invisible
to kernel-level I2C tracing, not one that merely didn't show up in
one capture window.

**PSU protocol details:**

- Address: `0x10` (7-bit). Confirmed two ways: (1) skot's
  `apw12-psu-tool` documents it as the default, and (2)
  independently, by disassembling the real APW12 PSU firmware
  (`github.com/HashSource/Antminer-APW12-Firmware`, PIC16F1704) —
  its I2C peripheral init sets `SSPADD = 0x10 << 1`, which on this
  PIC's MSSP hardware means the chip ACKs 7-bit address `0x10`.
- Write register: `0x11`.
- Framing (APW12 protocol, ported from
  `amlogic-cb-tools/src/protocol.rs`): `[0x55 (preamble LSB), 0xAA
  (preamble MSB), length, command, ...payload, checksum_lo,
  checksum_hi]`. `length = payload.len() + 4`. `checksum =
  sum(length, command, payload bytes) as u16`, split little-endian
  into the last two bytes. A single `0xF5` byte in place of a full
  frame means NAK.
- Transport mechanics: each byte of an outgoing frame is sent as its
  own full I2C transaction — `START, addr+W, ACK-check, register
  (0x11), ACK-check, data byte, STOP` — repeated once per frame byte.
  The response is read the same way in reverse: repeated
  single-byte current-address reads (`START, addr+R, read one byte,
  NAK, STOP`) until a full response frame (or a bare NAK byte) has
  been assembled.
- Known command bytes: `GET_FW_VERSION=0x01`, `GET_HW_VERSION=0x02`,
  `GET_VOLTAGE=0x03`, `MEASURE_VOLTAGE=0x04`, `READ_STATE=0x05`,
  `READ_CAL=0x06`, `WATCHDOG=0x81`, `SET_VOLTAGE=0x83`,
  `WRITE_CAL=0x86`.

A working, from-scratch Rust implementation of both the bit-bang
transport and this framing exists at
`mujina-miner/src/bin/psu_bitbang_probe.rs` in the `mujina` clone,
and has been verified end-to-end against the real PSU — see
the engineering log's PSU section for the verification detail and exact
output.

## Flash layout (for reference / recovery)

```
mtd0  bootloader     2MB   (raw dump incomplete — hit "no such device" partway through
                            raw NAND reads without ECC-aware tooling; not a concern,
                            never touched)
mtd1  tpl            8MB   (dumped clean)
mtd2  stock_system   50MB  (dumped clean)
mtd3  stock_config   5MB   (dumped clean)
mtd4  overlay        32MB  (dumped clean)
mtd5  system         153MB (dumped clean)
```

Backups and full BraiinsOS config files (`bosminer.toml`,
`bosminer-settings.json`, `bosminer-profile-grid.json`,
`bosminer-autotune.json`, `network.conf`) were pulled during the
original recon pass — not checked into this repo (may contain pool
credentials), regeneratable via the same `dd`-over-SSH approach in
[s19k-pro-recon-report.md](reference/recon-methodology.md) if ever needed.

## Relevance to a Mujina board driver

**Most of this is now implemented** — `board/antminer_s19k_am3.rs`,
`src/linux_hw/`, and `peripheral/apw12.rs` in the `mujina` clone. See
the engineering log's
["Native hw_trait implementations and the first working board driver"](reference/full-engineering-log.md#native-hw_trait-implementations-and-the-first-working-board-driver)
for what's actually working today vs. still planned; the description
below (written before that work) is now mostly a match rather than a
plan.

The hashboards use **BM1362** — the same chip Mujina's existing
`emberone00.rs` driver already speaks the wire protocol for (just
never finished the board-level integration). The work is a new
`VirtualBoardDescriptor`-based board for this AM3/Amlogic control
board:

- Drive GPIO 454/455/456 as chain enable, read 439/440/441 for
  presence.
- Speak BM13xx over `ttyS1`/`ttyS2`/`ttyS3` at 3,125,000 baud (77
  chips/chain) — the ASIC-level protocol code in
  `mujina-miner/src/asic/bm13xx/` is directly reusable.
- Read TMP75 sensors and EEPROMs over `/dev/i2c-1` (real hardware
  I2C, standard `i2c-dev` semantics — a normal `hw_trait::I2c`
  implementation backed by `/dev/i2c-1` covers this).
- Talk to the PSU over the **bit-banged** GPIO 476/477 bus using the
  APW12 protocol above — this does *not* fit a standard
  `hw_trait::I2c`-over-`/dev/i2c-N` implementation, since it isn't a
  real Linux I2C device; it needs its own software bit-bang driver
  (working reference code exists, see above).
- Drive fan PWM via `/sys/class/pwm/pwmchip0/{pwm0,pwm1}` and read
  tachometers via GPIO 447-450.

See the engineering log for the concrete architecture notes on how this maps
onto Mujina's actual source (as opposed to its stale
`board/README.md`), current build status, and next steps.

## Revision history

- **2026-08-24** (original recon pass): established the UART/GPIO/
  I2C map above through passive observation, correctly identified
  chain-enable, presence-detect, and the two live I2C busses.
  Incorrectly assumed the control board was `aarch64` userspace
  (kernel arch was taken at face value), guessed PSU comms might be
  serial-based on `ttyS0`, and left GPIO 437/438, 445/446, and
  476/477's functions only partially resolved.
- **2026-08-25** (this document): corrected the architecture
  (32-bit ARM userspace under a 64-bit kernel), found and fully
  solved PSU communication (bit-banged GPIO 476/477, not I2C-dev, not
  serial), obtained first-party vendor confirmation of the full GPIO
  map via `/etc/init.d/S37board_setup`, and confirmed the real APW12
  protocol end-to-end against the live PSU.
- **2026-08-25** (continued, same day): retried chip discovery with
  the PSU genuinely enabled and real voltage confirmed present —
  still 0 chip responses. Ruled out chain-enable state, baud rate,
  UART line settings, and a naive power-sequencing timing issue as
  explanations; confirmed the hardware itself is completely fine
  (`bosminer`'s own bring-up still works instantly on the same
  hardware state). Surfaced, but did not resolve, an open conflict
  in HB0/HB1/HB2-vs-chain-N UART numbering. See
  [the engineering log](reference/full-engineering-log.md#chip-discovery-still-silent-several-hypotheses-now-ruled-out)
  for the full investigation.
- **2026-08-25** (continued further, same day): found and fixed a
  real bug in the probe's command sequence (it was using a
  BM1370/S21-Pro-shaped sequence; the real captured BM1362 sequence
  documented in Mujina's own protocol reference is different — see
  the engineering log), and independently verified the exact wire bytes
  transmitted match the reference documentation byte-for-byte. Still
  0 chip responses. Also recorded a hard operating rule: never
  disable or risk disabling the cooling fans, for any reason —
  verify via `bosminer`'s live Prometheus metrics
  (`/etc/log/metrics/metrics.prom`), not GPIO tach polling, which is
  too slow to trust.
- **2026-08-25** (final round, same day): captured `bosminer`'s own
  PSU voltage-ramp log output, revealing its real operating target is
  **~15.2V**, not the ~12-13V level all prior chip-discovery testing
  had used. Added a `set-voltage` command to the PSU probe and
  retested chip discovery at the confirmed real voltage directly
  (verified via `measure-voltage`, not assumed) — still 0 responses.
  Hardware confirmed fully healthy afterward (fresh `bosminer`
  discovery, stable fan RPM). See
  [the engineering log](reference/full-engineering-log.md#round-3-found-the-real-voltage-target-152v-tested-at-it-directly-still-silent)
  for the full investigation.
