# S19K Pro (AM3/Amlogic) Recon — Plan & Results

> **This is the original recon session's record — kept as-is as a
> faithful account of what was actually done.** A follow-up session
> on 2026-08-25 corrected some of its conclusions (notably: the
> control board's userspace is 32-bit ARM, not aarch64 as assumed
> below, and PSU communication was fully solved — it's a bit-banged
> GPIO bus, not the I2C guess this session made). See
> [HANDOFF.md](full-engineering-log.md) for current status and
> [the hardware reference](../hardware.md)
> for the corrected, up-to-date hardware reference. The methodology
> and what-was-safe-vs-risky reasoning below remains accurate and is
> still the right way to think about further hardware work on this
> unit.

Session date: 2026-08-24. Target: an Antminer S19K Pro at `<miner-ip>`, running stock BraiinsOS+, owned by the user (accepted as a sunk cost for this exercise). Goal: figure out enough of this board's hardware interface to inform a future Mujina `BackplaneConnector` driver, in support of the community fork work referenced on the [256 Foundation forum](https://forum.256foundation.org/t/best-practices-for-hacking-mujina-onto-other-miners/48) (Schnitzel/skot/AgentP).

Root SSH access turned out to already be open on the box (BraiinsOS default) — no exploit was needed to get in.

## The plan (as approved before any risky action was taken)

The plan was staged from safest to riskiest, not out of excess caution but because each cheap step narrows down what the risky steps actually need to do.

**Phase 0 — Safety net (read-only)**
- Back up BraiinsOS config files (`bosminer.toml`, settings/profile/autotune JSON, `network.conf`)
- Raw-dump every `/dev/mtdXro` flash partition to a local file as a real recovery backup

**Phase 1 — Passive hardware mapping (read-only)**
- Read direction/value of every already-exported GPIO line without changing anything
- Look for a device-tree blob or live `/sys/firmware/devicetree/base` naming hardware functions
- Dump `/proc/iomem`/`/proc/interrupts` for peripheral clues

**Phase 2 — Passive correlation while mining continued (read-only)**
- Poll all GPIO values for ~2 minutes during normal operation, looking for lines that move in lockstep with known events (PSU poll cadence, etc.)

**Phase 3 — Controlled interruption (real risk begins, but recoverable)**
- Gracefully stop `bosminer`/`boser` via their own init scripts (not `kill -9`), so Braiins' shutdown code — not our guesswork — sequences the hashboards/PSU down safely
- Passively listen on each hashboard UART and the PSU UART for any idle chatter
- Attempt a passive BM13xx bus-scan per Mujina's own chip reference

**Phase 4 — GPIO experiments (the risk phase, one line at a time)**
- With mining stopped, toggle each still-unidentified GPIO line individually, observe the effect, revert
- Save anything power-rail-shaped for last, since that's the one most likely to cause a rough power cycle if wrong

**Phase 5 — Wrap-up**
- Reboot/restart, confirm mining resumes at baseline (or document that it didn't)
- Write up GPIO/UART/PSU findings as a reference doc for a future board driver

**Risk summary going in**: Phases 0–2 zero risk, Phase 3 low risk, Phase 4 the real risk — explicitly accepted given the unit is a sunk cost.

## What actually happened

Execution mostly followed the plan, with one deliberate deviation: **Phase 4's manual GPIO toggling turned out to be unnecessary.**

- **Phase 0**: Completed. 5 of 6 flash partitions dumped cleanly and match their declared sizes exactly. `mtd0` (bootloader) hit a "no such device" error partway through raw NAND reads — a known limitation of reading raw MTD char devices without ECC-aware tooling (`nanddump`), which isn't present on this BusyBox image. Not a concern: it's the smallest partition and the one we'd never touch anyway.

- **Phase 1**: Completed, with a negative result worth noting. `/sys/firmware/devicetree/base` was live and browsable (better than expected — no need to pull/decompile a `.dtb`), but it turned out to be generic Amlogic SoC boilerplate (`uart_A`/`uart_B`/`uart_AO`/`uart_AO_B`) with zero mining-specific labels. Confirmed 4 real UART peripherals exist, matching `ttyS0`–`ttyS3`, but the actual board wiring knowledge lives only inside Braiins' closed `bosminer-plus-tuner` binary.

- **Phase 2**: Completed. Of 17 exported GPIO lines, 8 were rock-steady across 2 minutes of normal mining (candidates for enable/presence lines), and the rest toggled too fast for 3-second polling to resolve — informative, but not conclusive on its own.

- **Phase 3**: `bosminer`/`boser` were stopped via `/etc/init.d/S99bosminer stop` (confirmed graceful — the script waits for the actual process to exit rather than just signaling and returning). Passive listening on all 4 UARTs for idle traffic returned **zero bytes** on every port — a genuine finding: the hashboards and PSU are pure slaves that never speak unless polled, so passive listening alone can't reveal their protocol. Building an active BM13xx query frame (correct preamble/CRC5) was judged out of scope for a recon pass without a working reference implementation on hand, so that sub-step was skipped rather than improvised unsafely.

- **Phase 4 — skipped by design, not by accident.** Stopping `bosminer` for Phase 3 incidentally did the job Phase 4 was for: comparing GPIO state immediately before and after the graceful stop showed **454/455/456 dropping from `1`→`0` in lockstep with the shutdown**, which is direct confirmation they're the three per-chain hashboard-enable lines — obtained from watching Braiins' own known-good shutdown code, not from us guessing which line to flip. Manually toggling lines ourselves would have added risk for strictly less confidence than this, so it was dropped from the plan once the better signal appeared.

- **Phase 5**: `bosminer` was restarted via its init script. All 3 hashboards re-detected, all 231 BM1362 chips re-enumerated (77 per chain, matching "expected 77"), all 6 temperature sensors re-bound, chain baud rate re-negotiated at 3,125,000 — confirmed via the live `bosminer.log`, not assumed. **The miner returned to its exact pre-test operating state.** Findings were written up to [the hardware reference](../hardware.md) in this same directory.

## Result

No manual, guess-and-toggle GPIO experimentation was ever performed — the plan's own staging (interrupt safely first, observe, only then consider manual toggling) surfaced enough signal that the highest-risk step became unnecessary. The unit was never actually put at risk beyond a temporary, deliberate, and fully-recovered mining pause.

**Key deliverable**: [the hardware reference](../hardware.md) — the technical reference (GPIO map, UART/baud/chip-count, I2C sensor addresses, flash layout, open questions) for whoever picks up writing the actual Mujina board driver for this hardware.
