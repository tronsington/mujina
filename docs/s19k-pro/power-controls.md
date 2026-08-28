# Power estimation and dynamic power limiting — design notes

Working notes for adding BraiinsOS-style **power targeting** to the
S19K Pro: you enter a wattage, firmware picks frequency and voltage to
hit it while maximising hashrate.

**Status: partially implemented.** Forward model lives in
`mujina-miner/src/power_estimate.rs` and is published on
`GET /api/v0/miner` as `boards[].powers[]` with
`source: "estimated"`, plus `frequency_mhz` / `chip_count`. Runtime
frequency control and the power-limit loop are still design-only.

## Measured baseline to design against

From a verified run (see [bring-up-log.md](bring-up-log.md), Round 15):

| | |
|---|---|
| Hashrate | ~105–115 TH/s (pool-verified) |
| Frequency | 575 MHz (compile-time constant) |
| PSU rail | 14.04 V measured (13.9 V target) |
| Per-domain voltage | 14.04 / 11 domains ≈ **1.276 V** |
| Chips responding | ~206 of 231 |
| Hottest board sensor | 49 °C at 100% fans |
| Overtemp cutoff | 75 °C (kills PSU) |
| Estimated power | ~2,000–2,270 W — **estimated, never measured** |
| Nameplate | 120 TH/s @ 2760 W = 23.0 J/TH |
| EEPROM ATE setpoint | 645 MHz @ 13.9 V |

## The physics

For CMOS, to a good approximation:

```
P_dynamic ≈ K · N · f · V²          switching power
P_leakage ≈ L · N · V · k^(T/10)    grows with temperature
Hashrate  ≈ c · N · f               linear in frequency
```

where `N` = active chips, `f` = chip frequency, `V` = core voltage,
`T` = die temperature.

Dividing gives efficiency:

```
J/TH ≈ (K/c)·V²  +  L/(c·f)
```

Two consequences that drive the whole design:

1. **Voltage is the efficiency lever** — it enters squared. Undervolting
   is where the real wins are.
2. **Higher frequency slightly *improves* J/TH**, because fixed leakage
   amortises over more hashes. Running at very low frequency is
   inefficient, which is why "just underclock it" is the wrong approach.

And the constraint that makes this non-trivial: **f and V are coupled.**
Every frequency has a minimum voltage `V_min(f)` below which timing
fails and chips return wrong hashes. Finding that curve *is* the job.
It is what bosminer's tuning phase is doing when it sweeps f and V after
ramp-up.

## Why the current power number cannot be used for control

The dashboard shows `P = hashrate × 23.5 J/TH`. That is **circular** —
it can only ever report back the efficiency constant it was given. It
cannot answer "what would power be at 500 MHz?", which is precisely the
question a power limit needs.

A power limit requires a **forward model** `P(f, V, T, N)` that can be
inverted for `f`. Different thing entirely.

## An anchored forward model

Anchor on the nameplate and ATE setpoint, then subtract the non-ASIC
loads:

```
P_asic_dc(nominal) ≈ 2760 × 0.93  −  135 (fans)  −  15 (control board)
                   ≈ 2417 W   @ 645 MHz, 13.9 V, 231 chips

P_asic(f, V, N) ≈ 2417 · (N/231) · (f/645) · (V/13.9)²
```

Sanity check at the measured operating point (575 MHz, 14.04 V, 206
chips):

```
2417 × (206/231) × (575/645) × (14.04/13.9)²
  = 2417 × 0.892 × 0.891 × 1.020
  ≈ 1,960 W DC   →  +150 W fans/control  →  /0.93  ≈  2,270 W AC
```

Close to the circular estimate (~2,000 W), but this version is
**causal** — it predicts unmeasured operating points.

**Refinement once there is data to fit:** split dynamic from leakage
(roughly 85/15 at nominal) so temperature enters properly:

```
P ≈ P_dyn0·(N/231)(f/645)(V/13.9)²  +  P_leak0·(N/231)(V/13.9)·k^((T−T0)/10)
```

`k` ≈ 1.3–1.5 per 10 °C is the usual ballpark. Do not trust these
coefficients without fitting them against real measurements.

## Getting real numbers — three options, ranked

### 1. A metering smart plug (do this first)

$15–30: Shelly Plug S, any Tasmota plug, Kasa KP115. Polls over HTTP,
drops straight into the dashboard aggregator next to everything else.

This is the highest-value step by a wide margin:

- Converts the model from a guess into a **fitted** model
- Measures fan power for free (step fans, watch watts)
- Gives real J/TH, so efficiency claims become defensible
- Anchors any future power-target feature in something true

Without it, every number here is unvalidated. This project has already
lost hours to building theories on an unvalidated measurement — see
[lessons.md](lessons.md).

### 2. Ask the PSU — it may already know

Two concrete unexplored leads:

- **`READ_CAL = 0x06` is documented but not implemented.** Our driver
  (`mujina-miner/src/peripheral/apw12.rs`) implements only
  `GET_FW_VERSION 0x01`, `GET_HW_VERSION 0x02`, `GET_VOLTAGE 0x03`,
  `MEASURE_VOLTAGE 0x04`, `READ_STATE 0x05`, `WATCHDOG 0x81`,
  `SET_VOLTAGE 0x83`. Calibration blobs typically carry ADC scaling
  constants — **if there is current-sense calibration in there, current
  sensing exists.**
- **The PSU firmware has already been disassembled by someone.**
  `github.com/HashSource/Antminer-APW12-Firmware` (PIC16F1704) is cited
  in our own hardware notes as the source used to derive this protocol.
  That disassembly is the definitive answer to "can the APW12 report
  current?" — and reading it costs nothing and risks nothing.

If the PSU can report current, this entire problem collapses to a
solved one. **Check this before building any model.**

Caution if probing command IDs on real hardware: `0x01`–`0x06` appear to
be reads and `0x81`/`0x83`/`0x86` writes (high bit set). Probing
unknown *write* commands on a live 2 kW PSU is a genuinely new risk
category — do not do it casually.

### 3. Model only

Adequate for *relative* statements ("this change cut power ~8%"), not
for absolute ones. If shipping this, label it estimated everywhere, as
the dashboard currently does.

## Blockers in the current stack

| Lever | Status |
|---|---|
| **Frequency** | **Compile-time only.** `target_frequency_mhz: Some(575.0)` in the reference's `asic/bm13xx/chip_config.rs` `bm1366()`. No config key, no env var, no API route. **This is the hard blocker** — power targeting is impossible without runtime frequency control. |
| **Voltage** | Runtime-settable and working, but clamped to 13.9–14.5 V (`voltage_range()` in `board/s19k_pro_amlogic.rs`). ±4% on V is only ~±8% on power via V² — nowhere near enough range on its own. |
| **Per-board control** | Not possible. One shared APW12 rail feeds all three boards (11 series domains each), so voltage is global. Unlike multi-rail units, boards cannot be tuned independently. |
| **Hardware-error signal** | **Missing entirely** — and required for safe undervolting. See below. |

The voltage-range point deserves emphasis: with `f` fixed and `V`
confined to 13.9–14.5 V, **there is currently almost no power
adjustment range at all.** Runtime frequency control is mandatory, not
optional.

For scale, once `f` is controllable: sweeping 400–645 MHz gives roughly
1,500–2,400 W of ASIC power — a genuinely useful range.

## The missing piece: a hardware-error signal

Safe undervolting needs to know when chips start computing *wrong*, and
there is a clean signal available that nothing currently uses.

Mujina already recomputes every returned nonce's hash locally to check
it against the share target. A legitimate but sub-target nonce still
has **≥32 leading zero bits** (the chip's own filter guarantees it). A
genuinely miscomputed one will not.

> **HW error rate = fraction of returned nonces whose locally
> recomputed hash has fewer than 32 leading zero bits.**

Properties that make this the right signal:

- **Fast** — thousands of nonces per second, so it responds in seconds
  rather than the minutes a pool-side signal would take
- **Unambiguous** — distinct from pool rejects, which on this setup are
  dominated by stale/vardiff effects (Round 15) and say nothing
  about chip stability
- **Cheap** — the hash is already being computed; only the leading-zero
  count and a counter are new

Implementation site: the nonce handler in the reference's
`asic/bm13xx/thread_v2.rs`, in the same block that currently logs
"Nonce does not meet target". Expose it in `/api/v0/miner` and graph it
on the dashboard.

## Control loop design

Since `H ∝ f`, the objective is: **the highest frequency that fits the
power budget, at the lowest voltage that is stable at that frequency.**

1. From target `P*`, invert the forward model for `f_max`
2. Look up `V_min(f)` from a learned per-machine profile, add a margin
3. Ramp to the new point — **reuse the existing 6.25 MHz / 100 ms ramp,
   never step frequency**
4. Slow outer loop:
   - HW error rate above threshold → raise V, or drop f
   - Zero errors with power headroom remaining → try one step up
5. Persist the learned V–f profile so it is not relearned every boot

This mirrors what bosminer does, and matches the behaviour observed on
this unit: ramp first, then hunt (f, V) for efficiency at a power
target.

### Safety rails

- **Hard absolute voltage clamp.** Overvolting is the damaging
  direction — heat and electromigration. Undervolting mostly produces
  errors and instability. Per-domain voltage is the number that matters
  to the silicon (`rail / 11`); confirm BM1366 spec before widening the
  range beyond the reference's 13.9–14.5 V.
- **Temperature is the real backstop, not the model.** A model error
  must never be able to cook the machine.
- **Leave the existing overtemp gate untouched.** The PSU-kill in
  `native_telemetry_task` is the last line of defence and should be
  treated as sacred.
- **Ramp, never step**, on both frequency and voltage.
- Fail-safe on sensor loss: unreadable or implausible telemetry → back
  off to a known-safe conservative point, never "hold last value".

## Recommended order of work

1. **Ground truth.** Order a metering plug. Meanwhile read the APW12
   firmware disassembly and implement `READ_CAL (0x06)`. Either might
   hand us real current, which changes everything downstream.
2. **Instrument.** Add the HW-error metric. Expose frequency and
   voltage in `/api/v0/miner` (neither is exposed today). Both are
   low-risk, additive, and useful on their own.
3. **Runtime frequency control.** The real engineering work: a command
   channel to the ASIC thread to re-run the ramp to a new target
   frequency without a rebuild. Wire it to `PATCH /miner`.
4. **Fit the model** against plug measurements across several (f, V)
   points.
5. **Then** build the power-target tuner.

Steps 1–2 are safe and make everything afterwards verifiable. Step 3 is
where the interesting work starts. Do not build step 5 first.

## Code locations

Reference port ([Schnitzel/mujina](https://github.com/Schnitzel/mujina),
branch `amlogic-s19kpro-support`, under `mujina-miner/src/`):

| What | Where |
|---|---|
| Frequency target (compile-time) | `asic/bm13xx/chip_config.rs` — `bm1366()`, `target_frequency_mhz` |
| Frequency ramp (reuse this) | `asic/bm13xx/sequencer.rs` — `build_frequency_ramp()`; executed in `thread_v2.rs` |
| Voltage target and range | `board/s19k_pro_amlogic.rs` — `target_voltage()`, `voltage_range()`, `voltage_step()` |
| Overtemp gate (do not touch) | `board/s19k_pro_amlogic.rs` — `native_telemetry_task` |
| Nonce verification (add HW-error metric here) | `asic/bm13xx/thread_v2.rs` — the "Nonce does not meet target" branch |
| Fan PWM write | `board/s19k_pro_amlogic.rs` — `configure_fans()`; driver in `amlogic-cb-tools/src/pwm.rs` |
| Unwired fan command stub | `api/commands.rs` — `BoardCommand::SetFanTarget`; `api_client/types.rs` — `SetFanTargetRequest` |
| API routes | `api/v0.rs` — `routes()`; only mutation today is `PATCH /miner {paused}` |

This repo:

| What | Where |
|---|---|
| PSU driver / command set | `mujina-miner/src/peripheral/apw12.rs` |
| Bit-bang I2C to PSU | `mujina-miner/src/linux_hw/bitbang_i2c.rs` |
| Our own board driver | `mujina-miner/src/board/antminer_s19k_am3.rs` |

## Hardware cheat-sheet

- 3 × BHB56902 hashboards, 77 BM1366 chips each (231 total)
- **11 voltage domains in series per board** — per-domain V = rail / 11
- One shared APW12 rail (APW121215d/e, hw `0x75`, fw `0x0019`)
- PSU is on **bit-banged GPIO 476/477**, not any `/dev/i2c-N`
- PSU enable: GPIO 437, active-low (`1` = disabled)
- 6 × TMP75 board sensors on `/dev/i2c-1`, (inlet, outlet) per board
- 4 fan channels on 2 PWM channels: fans 0&1 → `pwm0`, fans 2&3 →
  `pwm1`. Fan 3 reads 0 RPM. Stock `bosminer` read it as 0 as well
  (`fan_rpm_feedback{idx="3"} 0` in its own metrics while 0/1/2 ran
  ~6,000 RPM), so this is a **pre-existing hardware fault, not
  something Mujina's fan control causes** — and not a dead PWM
  channel, since `pwm1` also drives fan 2, which spins. Whether the
  fan is stopped or only its tachometer is unread is unconfirmed;
  both stacks read the tach through the same path. Treat the unit as
  running on three of four fans when judging thermal headroom
- PWM: `configure_percent(period, percent, enable)`, polarity `normal`,
  duty scales linearly with percent, sense confirmed not inverted
  (50% → ~4.2–5.4k RPM, 100% → ~6.3–7.9k RPM)
- Overtemp cutoff 75 °C; observed 49 °C at 100% fans and ~105 TH/s
- **Never disable the fans.** Not as a test, not briefly.

## Appendix: dynamic fan control

Deliberately sequenced *after* power control, since fan power is small
next to the ASIC and the fan curve depends on the thermal envelope
power sets.

Motivation: 49 °C against a 75 °C cutoff means **26 °C of headroom is
being spent on nothing.** Fans at 100% are ~135 W of ~2,000 W; fan
power goes roughly as RPM³, so ~65% would be ~40 W. Saves ~95 W (~5%),
though ASIC leakage rises as it runs warmer, so the net is smaller.
Noise is the bigger real-world win — 100% → 65% is roughly 8–12 dB.

**Recommended approach: a curve, not a PID.** A lookup table with
interpolation, a hard floor, and a latched panic. PID on a slow thermal
mass with noisy sensors invites hunting and integral windup for no
benefit. Add a slow integral trim later only if it proves necessary.

Control input: **max of all six sensors** (effectively the hottest
outlet).

Conservative starting curve:

| Max board temp | Fan % |
|---|---|
| ≤ 45 °C | 45 (floor) |
| 50 °C | 55 |
| 55 °C | 65 |
| 60 °C | 78 |
| 65 °C | 90 |
| ≥ 68 °C | **100, latched** |

Invariants:

- Never 0%, never touch `enable`. Floor ~45%.
- **Silence means full speed** — failed, implausible, or stale sensor
  reads → 100%. Never hold last value.
- Latched panic with hysteresis (100% above 68 °C; release only below
  ~60 °C sustained 30 s) so it cannot oscillate at the boundary.
- Slew limit ~10%/s; deadband ~1.5 °C to stop chatter.
- Heartbeat: if the fan loop stops updating, something still alive
  forces 100%. Note PWM persists in hardware, so a hung loop otherwise
  leaves fans frozen at their last value while temperature climbs.

The existing PSU overtemp gate is an independent backstop, so even
total fan-control failure still has hardware protection.

**Rollout:** shadow mode (compute and log, actuate nothing) → bounded
actuation (clamp 75–100%) → full range. Then deliberately test the
failure paths: temporarily lower the panic threshold to something
reachable and confirm it slams to 100%; point a sensor read at a bogus
address and confirm fail-safe. **Never test by stopping fans.**

**First step, before any controller: a characterisation sweep.** Step
fans 100 → 90 → 80 → 70 → 60 → 50%, hold each until temperatures
plateau (~5 min), recording fan %, per-fan RPM, all six temperatures,
hashrate, and HW-error/reject rate. Abort on 68 °C. That yields a real
thermal curve for this machine at this ambient and workload — and
answers a question currently unanswerable: **does hashrate or error
rate degrade as it heats up?** If errors climb at 65 °C, that is the
true ceiling, not the 75 °C cutoff.

## Supporting tooling already built

- **Dashboard aggregator** at [`tools/miner-dashboard/`](../../tools/miner-dashboard/) —
  polls mujina's API, the pool API, and HashScope; keeps ~2 h of
  history; serves `/` (plain), `/mmbn` (themed), `/builtin` (mujina's
  own, proxied), `/api/state` (JSON). Natural home for plug wattage,
  HW-error rate, and fan % once they exist.
- **Note:** mujina's API binds `127.0.0.1` by default and the miner's
  firewall is `INPUT policy DROP` with an allowlist excluding 7785, so
  reaching it from the LAN needs either `MUJINA_API_LISTEN=0.0.0.0:7785`
  plus a firewall rule, or an SSH tunnel
  ([`tools/miner-dashboard/tunnel.sh`](../../tools/miner-dashboard/tunnel.sh)).
- **HashScope** stratum MITM proxy for per-submit accept/reject.
- **Supervisor** — [`tools/miner-supervisor/mujina-supervisor.sh`](../../tools/miner-supervisor/mujina-supervisor.sh), which keeps
  mujina running unattended with a clean PSU cycle between attempts,
  crash-loop backoff, and a `bosminer` fallback. Needed for the long
  unattended runs that characterisation and tuning will require. See
  [running-it.md](running-it.md#unattended-operation).
- **[`reference/s19k-fixes.patch`](reference/s19k-fixes.patch)** — the
  fixes that made the reference port reach ~105 TH/s on this hardware.
  See [ATTRIBUTION.md](ATTRIBUTION.md) before redistributing it.
