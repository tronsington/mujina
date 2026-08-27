# Mujina on the Antminer S19K Pro (BHB56902 / BM1366)

Bringing [Mujina](https://github.com/256foundation/mujina) up on an
Antminer S19K Pro with the Amlogic A113D ("AM3") control board,
replacing stock BraiinsOS+ `bosminer`.

**Status: working.** Sustained **~105–115 TH/s** with a 0% reject
rate at settled pool difficulty, verified against the mining pool's
own accounting — versus **104 TH/s** measured from stock `bosminer`
on the same physical unit.

## The short version

Three hashboards, 77 BM1366 chips each (231 total), one shared APW12
PSU rail. Chip discovery, register bring-up, pool connectivity, and
job dispatch were all working relatively early — the chips
enumerated 77/77, accepted every register write, and returned real
nonces.

But real throughput sat at roughly **2–3 TH/s**, about 2% of what
the hardware should do, and for a long time it looked like a hard
frequency ceiling: nothing above ~300 MHz appeared to hash at all.

The actual cause was a **single wrong byte order**:

> BM1366's `Core` register (`0x3C`, `CORE_MAILBOX`) values were
> stored little-endian, but that one register is encoded
> **big-endian**. The chips received `40 85 00 80` instead of
> `80 00 85 40` — with **bit 31 clear**, which is `CORE_MAILBOX`'s
> "apply to all cores" flag. So every core write, including
> core-enable `0xAA`, addressed a single garbage `core_id` instead
> of the whole chip.

Fixing that took the miner from ~2.9 TH/s to ~105 TH/s. Full
analysis in [root-cause.md](root-cause.md).

## Measured results

| Metric | Before fix | After fix |
|---|---|---|
| Reported nonce rate | ~170/s | ~4,950/s |
| Hashrate (pool's own 5-min rate) | 0 | **~105–115 TH/s** |
| Accepted shares | 0 | continuous, **0% reject** at settled vardiff |
| Best difficulty seen | ~1,000 | **56,300,237** |
| Board temperature | 75 °C (tripped overtemp) | **49 °C**, flat over 45 min |
| Efficiency | order ~1,000 J/TH | **~23 J/TH** (nameplate: 120 TH/s @ 2760 W = 23.0 J/TH) |

Reference points on the same unit: stock `bosminer` measures
**104 TH/s** (1.88 s/share at difficulty 45,659 — internally
consistent: `45659 × 2³² / 1.04e14 = 1.885 s`). The community S19K
Pro fork this work built on claims 39.68 TH/s at 575 MHz.

A useful independent confirmation of the mechanism: **before** the
fix, ~2 TH/s with fans at 50% reached 75 °C and tripped the overtemp
cutoff. **After** the fix, ~105 TH/s at 100% fans sits at 49 °C. A
40× hashrate increase cannot produce less heat — the chips were
already burning roughly the same power, switching at frequency but
doing almost no useful work with it.

## Documents

| Document | What's in it |
|---|---|
| [hardware.md](hardware.md) | The hardware reference: GPIO map, UARTs, I2C buses, PSU protocol, chip topology, voltage domains. Start here for anything physical. |
| [bring-up-log.md](bring-up-log.md) | The round-by-round account of **what was tried and what failed**, and what each failure ruled out. This is most of the value — the dead ends are documented deliberately. |
| [root-cause.md](root-cause.md) | The `Core` byte-order bug in depth: why it survived code review and CI, why it produced exactly these symptoms, and how it was found. |
| [running-it.md](running-it.md) | Build, deploy, configure, run, and — importantly — how to *measure* hashrate correctly on this hardware. |
| [lessons.md](lessons.md) | Debugging and measurement traps hit during this work. Several cost hours. Worth reading before doing similar work. |

### Reference material

| File | What it is |
|---|---|
| [reference/full-engineering-log.md](reference/full-engineering-log.md) | The complete unedited working log, ~2,250 lines, written as work happened. The narrative docs above are distilled from this. Includes exact wire bytes, captured sequences, and reasoning at the time. |
| [reference/recon-methodology.md](reference/recon-methodology.md) | The original hardware recon session — how the hardware map was derived, and the safe-vs-risky reasoning for poking at a live miner. Some conclusions superseded; the methodology holds. |
| [reference/s19k-fixes.patch](reference/s19k-fixes.patch) | The fix as applied to the community S19K Pro fork (see below), self-contained across 5 files. Its header records the upstream repo, branch, base commit and licence — it patches *their* code, not this repository's. |
| [reference/mujina-s19k-real.toml](reference/mujina-s19k-real.toml) | Board config for this unit — chain/UART/GPIO mapping, PSU, fans. Note this is the **reference port's** config format; this repository's driver holds the same mapping as compile-time constants. |
| [ATTRIBUTION.md](ATTRIBUTION.md) | What came from whose project, under what licence, and where it landed. Read before lifting code from, or redistributing, any of this. |

## Two codebases, and which one is proven

This is worth being explicit about, because it affects what you can
rely on.

1. **This repository's own driver** —
   `mujina-miner/src/board/antminer_s19k_am3.rs` plus
   `asic/bm13xx/thread.rs`. Built from scratch against captured wire
   traces from `bosminer`. Mines real accepted shares and is
   confirmed solid at ~300 MHz (~4–6 TH/s), but **has not yet been
   retested since the root cause was found**. Its `Core` values come
   from this board's own captured table rather than the shared chip
   config, so it may already emit correct bytes — that needs
   checking.

2. **[Schnitzel/mujina](https://github.com/Schnitzel/mujina),
   `amlogic-s19kpro-support` branch** — an independently developed
   S19K Pro port. **This is where ~105 TH/s was demonstrated**, after
   applying [the patch](reference/s19k-fixes.patch): the `Core`
   byte-order fix, plus a bit-banged-GPIO PSU shim needed because
   this unit's PSU isn't reachable via any `/dev/i2c-N` device.

So: the hardware is proven to reach ~105 TH/s, and the root cause is
understood and fixed — but porting that fix into this repository's
own driver is still open. See [running-it.md](running-it.md).

## Known-open items

- **Find what else the reference port does differently.** The `Core`
  fix is in this repo's driver, and it was retested on real hardware at
  575 MHz on 2026-08-27: full bring-up, 231/231 chips, ramp to target,
  pool connected — and zero nonces, board flat at ambient. So the fix
  alone is not sufficient here. Round 15 proves the hardware reaches
  ~105 TH/s, so this remains a driver bug rather than a hardware limit,
  but the specific difference is still unidentified. Diffing this
  driver's wire output against the reference's, on the same hardware,
  is the obvious next move.
- Longer soak testing. ~45 minutes is clean and thermally flat; hours
  are untested (memory growth, pool reconnect, chip-count drift).
- Frequency headroom above 575 MHz is unexplored. The EEPROM ATE
  setpoint is 645 MHz and `bosminer` operates at 583–600 MHz. The
  earlier conclusion that ">300 MHz doesn't work" was an artifact of
  the `Core` bug and is void.
- No PLL readback exists. Whether chips actually reach the commanded
  frequency is still unverified by direct measurement — register
  `0x08` has never been read back on this hardware.

## Safety note

This is a ~2 kW machine that moves real heat. Two hard rules learned
here:

- **Never disable the fans.** Not as a test, not briefly.
- The reference port has **no dynamic fan control at all** —
  `default_fan_percent` is set once at startup and never revisited.
  At 50% it hit the overtemp cutoff in about four minutes. Use 100%
  for any sustained run.
