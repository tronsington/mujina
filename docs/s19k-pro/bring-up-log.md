# Bring-up log: what was tried, what failed, and what it ruled out

This is the condensed narrative of getting Mujina onto an Antminer
S19K Pro. It is deliberately weighted toward **failures**, because
the dead ends are what cost the time, and knowing what has already
been eliminated is most of the value for anyone continuing.

The unedited working log — with exact wire bytes, captured
sequences, and reasoning as it happened — is in
[reference/full-engineering-log.md](reference/full-engineering-log.md).

Two long-running problems structure the whole story:

1. **Getting chips to respond at all** (Rounds 1–5)
2. **Getting them to actually hash at a useful rate** (Rounds 7–15)

---

## Part 1 — Getting the chips to talk

### Round 1: power, GPIO, and timing

Established real PSU power-up, GPIO numbering, and reset timing on a
live unit. No chip responses yet. Mostly groundwork: figuring out
which sysfs GPIO lines correspond to which physical function, and
that the PSU is *not* on any `/dev/i2c-N` bus.

### Round 2: a real protocol bug fixed — still silent

Found and fixed a genuine sequencing bug, and verified the emitted
wire bytes matched the reference byte-for-byte. Chips still returned
nothing.

**Ruled out:** wire framing and CRC as the blocker.

### Round 3: found the real voltage (~15.2 V) — still silent

Captured `bosminer`'s own PSU ramp: 12.722 V climbing to 15.200 V in
decreasing steps. Tested at that voltage directly. Still silent.

**Ruled out:** insufficient voltage as the sole blocker. (Note: 15.2 V
later turned out to be the *bring-up ceiling*, not the sustained
operating point — see Round 14-continued.)

### Round 4: wire capture via ptrace — the command order was backwards

Built `s19k-trace`, a ptrace-based syscall tracer, and captured a
real successful `bosminer` bring-up. This was the turning point for
Part 1.

The captured order was structurally different from what had been
assumed:

```
55 aa 51 09 00 a4 90 00 ff ff 1c   VersionMask broadcast (×3, byte-identical)
55 aa 51 09 00 a8 00 07 00 00 03   broadcast InitControl (0xa8)
55 aa 51 09 00 18 ff 0f c1 00 00   broadcast MiscControl (0x18)
55 aa 53 05 00 00 03               ChainInactive (×3)
55 aa 40 05 00 00 1c               SetChipAddress 0x00
55 aa 40 05 02 00 01               SetChipAddress 0x02 … through 0x9a (77 chips × 2)
55 aa 41 09 8a 58 02 11 41 11 0f   per-chip writes (reg 0x58, 0x2c) — NOT in address order
55 aa 52 05 00 00 0a               discover — THE LAST STEP, not the first
```

**Discovery runs *after* every chip has been individually addressed
and configured**, not before. Every previous attempt had it first.

### Round 5: 77/77 chips respond

Two hard requirements, both found by A/B testing on real hardware:

- **All three chain-enable GPIOs must be asserted together.** Chain 3
  alone, at correct voltage, with the correct sequence: **zero**
  responses. Chains 1+2 also enabled, everything else identical:
  **77/77**. This is non-obvious and load-bearing.
- **A real reset pulse**: drive chain-enable low for 200 ms, high,
  then wait 2 s before any UART traffic.

Also: the chips **self-report as BM1366**, not BM1362 as inherited
from the S19j Pro reference.

**Discovery: solved.** Clean 77/77 on all three chains.

---

## An interlude: EEPROM corruption, and repair

During this period a hashboard EEPROM was corrupted and had to be
repaired. Documented in the full log. Worth knowing it happened, and
that it was recoverable.

---

## Part 2 — Getting them to hash

Rounds 6 and 7 wired discovery into the real board driver, created
real hash threads per chain, and connected to a live Stratum pool.
Jobs dispatched correctly. Chips returned **zero** nonces.

This is where it got hard, and where most of the time went.

### Round 8: baud switch tested — no change, but a real bug found

Tested switching to `bosminer`'s real 3.125 Mbaud operating speed.
No change to the silence, but a genuine corruption source was found
and removed along the way.

### Round 9: the `Core` corruption explained — a real endianness bug (decode side)

A post-`Core`-write byte flood turned out to be **885 perfectly
gapless, CRC5-valid frames**, each decoding as a `ReadRegister`
response for unknown register `0x40`, from real chip addresses, each
with its own slowly incrementing counter.

Root cause: `Register::decode()` interprets wire bytes little-endian,
but `Core`'s `encode_data` uses big-endian. The malformed
`CORE_MAILBOX` command (`wr=0`, out-of-range `core_id`) had put the
mailbox state machine into streaming telemetry.

**Note this carefully — the same endianness asymmetry causes the
Round 15 root cause, six rounds later, on the encode side.** It was
fixed here for decode only.

### Rounds 10 and 11: the documented per-chip core-enable pass — still silent

Added the per-chip `Core` core-enable pass that the protocol docs
describe. Then the full documented per-chip
`InitControl`/`MiscControl`/`Core` pass. Tested against a real pool.

Still zero nonces. Notably, board temperature stayed **completely
flat** at 25–28 °C — if cores were newly drawing real hashing power,
some rise would be expected.

**In hindsight:** this was the bug from Round 15 already in effect.
The core-enable writes were going out with bit 31 clear and doing
nothing.

### Round 12: SOLVED (partially) — first accepted share

Extracted the **real per-chip values** from the existing Round 4
trace, rather than using the generic documented ones:

- Per-chip `InitControl` = `00 07 01 f0` (broadcast is `00 07 00 00`)
- Per-chip `MiscControl` = `f0 00 c1 00` (broadcast is `ff 0f c1 00`)
- Per-chip `Core` triplet, in **real captured order**:
  1. `80 00 80 20` — clock delay
  2. `80 00 82 aa` — **core enable**
  3. `80 00 85 40` — clock select

The previously guessed order was clock-select → clock-delay →
core-enable. The real order is **clock-delay → core-enable →
clock-select**.

Result: **229 nonces in the first ~15 s, 756 by ~70 s, and Mujina's
first accepted share.** At ~50 MHz.

### Round 13: a real soak test, and a monitoring gap

~16 minutes at ~50 MHz. Stable. Caught a real safety-monitoring gap:
the established fan check reads a metrics file that **only updates
while `bosminer` is running** — it goes silently stale the moment
`bosminer` is stopped to run Mujina. A stale reading looks identical
to a fresh one. *Always check the timestamp, not just the value.*

### Round 14: 300 MHz works, nothing above it does

300 MHz confirmed solid: real rising temperature, real accepted
shares, ~4–6 TH/s measured. Every frequency above it — 350, 420, 525,
575, 590 MHz — failed **identically**: flat temperature, zero nonces.

Six hypotheses tested individually, all negative:

| Hypothesis | Result |
|---|---|
| PLL divider choice (two different search strategies) | Ruled out |
| Ramp granularity (50 MHz steps → 6.25 MHz) | Ruled out |
| VCO stability mid-ramp | Ruled out |
| PLL settle time | Ruled out |
| Post-divider preservation across the ramp | Ruled out |
| A missing/incorrect `NonceRange` | Ruled out |

A misleading signal from this round, worth flagging: a standalone
probe swept 100–600 MHz writing `PllDivider` and re-running discovery
at each step, and reported **clean 77/77 at every frequency**. That
looked like a green light. It only proves the chain still *addresses*
cleanly — it says nothing about PLL lock or whether cores are
hashing. It gave false confidence for a long time.

### Round 14 (continued): an external reference, three sourced fixes — still nothing

Found [Schnitzel/mujina](https://github.com/Schnitzel/mujina)'s
`amlogic-s19kpro-support` branch — an independent S19K Pro port
claiming 39.68 TH/s. It yielded three real, well-sourced corrections:

1. **BM1366 requires strictly `post_div1 > post_div2`** (not `>=`),
   with `fb_div` in `0x90–0xEB`. Sourced from ESP-Miner's real BM1366
   firmware. The shared search allowed equality, so several target
   frequencies resolved to electrically invalid divider pairs.
2. **Real operating voltage is 13.9 V**, not 15.2 V. Braiins' own log
   on this hashboard: `Voltage (Avg.) 13.90 V, Frequency (Avg.)
   645 MHz`. Every test so far had run the whole time at the 15.2 V
   *bring-up ceiling*.
3. **The frequency ramp runs last**, after `TicketMask`, `NonceRange`
   and the baud switch — not before them.

All three were applied together and individually verified correct on
the wire. **Still zero nonces at 575 MHz.**

Three genuinely real bugs fixed, no change in outcome. This is the
most frustrating kind of result, and the point at which the approach
had to change.

### Round 15: SOLVED — the `Core` byte-order bug

The change in approach that worked: **stop generating hypotheses from
symptoms, and systematically read the reference source against the
real captures instead.**

Every theory produced by staring at symptoms had been wrong. Reading
code found it in about an hour.

`Register::Core` is encoded **big-endian** (`put_u32`) while every
other raw register is little-endian (`put_u32_le`). The BM1366 chip
config stored its `Core` values in little-endian order:

```rust
core_broadcast: [0x4085_0080, 0x2080_0080],                  // wrong
core_perchip:   [0x4085_0080, 0x2080_0080, 0xAA82_0080],     // wrong
```

Big-endian-encoded, `0x4085_0080` reaches the chip as
`40 85 00 80` — the exact byte-reverse of the intended
`80 00 85 40`, with **bit 31 clear**. Bit 31 is `CORE_MAILBOX`'s
"apply to all cores" flag.

`bm1362()` and `bm1370()` stored the same registers correctly, so
only BM1366 was affected — and the guarding unit test asserted
`to_le_bytes()` for *all* registers including `Core`, so CI was green
over it.

Result: **~2.9 TH/s → ~105 TH/s**, 0% reject at settled difficulty,
49 °C. Full analysis in [root-cause.md](root-cause.md).

---

## What the failures actually ruled out

Consolidated, since this is the reusable part:

- **Wire framing, CRC, command encoding** — correct since Round 2.
- **Chip discovery and addressing** — solved Round 5; needs all three
  chain GPIOs asserted together and a real reset pulse.
- **Job dispatch, merkle root computation, header reconstruction,
  nonce/job correlation** — all verified correct. Zero
  `unknown job_id` and zero merkle failures across 21,500+ samples.
- **Share submission pipeline** — proven end-to-end against a dummy
  job source (101 shares, best difficulty 254,808) well before the
  real fix. The pool path was never broken.
- **PLL divider math, ramp granularity, settle time, VCO stability**
  — all tested individually, all negative.
- **`NonceRange` partitioning** — a "fix" here was actively a
  regression; real firmware broadcasts a single value and still
  reaches ~39 TH/s.
- **PSU voltage** — 13.9 V is correct, but was not the blocker.

What none of those touched, and what actually mattered: whether the
`Core` register writes were **reaching all cores at all**.
