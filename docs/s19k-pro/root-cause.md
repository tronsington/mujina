# Root cause: a byte-order bug on the BM1366 `Core` register

**One wrong byte order cost roughly 50× the hashrate**, and produced
symptoms that looked convincingly like a frequency or PLL problem for
several rounds of debugging.

## The bug

`Register::Core` — register `0x3C`, the `CORE_MAILBOX` — is encoded
**big-endian**, while every other raw register is encoded
little-endian. From `mujina-miner/src/asic/bm13xx/protocol.rs`:

```rust
Register::Core { raw_value } => {
    // Core register needs big-endian encoding
    dst.put_u32(*raw_value);
}
Register::MiscControl { raw_value }
| Register::UartRelay { raw_value }
| Register::InitControl { raw_value }
| /* ...every other raw register... */ => {
    dst.put_u32_le(*raw_value);
}
```

That asymmetry is real and correct — it matches captured wire bytes
and is covered by a capture-backed test.

The BM1366 chip config stored its `Core` values in **little-endian
byte order**, as if they'd be encoded with `put_u32_le`:

```rust
// chip_config.rs — bm1366(), BEFORE
core_broadcast: [0x4085_0080, 0x2080_0080],
core_perchip:   [0x4085_0080, 0x2080_0080, 0xAA82_0080],
```

Big-endian-encoded, `0x4085_0080` goes onto the wire as:

```
40 85 00 80        ← what the chips actually received
80 00 85 40        ← what the comment above it said was intended
```

An exact byte-reverse.

### Why that destroys hashrate

`PROTOCOL.md` documents bit 31 of `CORE_MAILBOX` as always set in
observed implementations. It is the **"apply to all cores"**
broadcast flag.

In `80 00 85 40`, bit 31 is **set** → the write applies to every core
on the chip.
In the reversed `40 85 00 80`, bit 31 is **clear** → the write is
addressed to one individual `core_id`, derived from what are now
essentially garbage bits.

So every core configuration write — clock delay, clock select, and
critically **core enable `0xAA`** — was landing on a single wrong
core instead of the whole chip. The vast majority of cores were never
enabled.

### The fix

Store them in the order the encoder actually expects, exactly as
`bm1362()` and `bm1370()` already did:

```rust
// chip_config.rs — bm1366(), AFTER
core_broadcast: [0x8000_8540, 0x8000_8020],
core_perchip:   [0x8000_8540, 0x8000_8020, 0x8000_82AA],
```

## Why it survived for so long

Four independent reasons, each worth internalizing:

**1. Only BM1366 was affected.** `bm1362()` and `bm1370()` stored the
same registers in natural big-endian form and were fine. So the S21
Pro and S19j Pro paths worked, and the bug looked like "the S19K Pro
is different somehow" rather than "this value is wrong."

**2. The guarding unit test asserted the wrong encoder.**

```rust
// BEFORE — checks to_le_bytes() for ALL registers, including Core
assert_eq!(raw.to_le_bytes(), *expected, ...);
```

With `raw = 0x4085_0080`, `to_le_bytes()` yields
`[0x80, 0x00, 0x85, 0x40]` — exactly the expected value. **The test
passed while the wire got the reverse.** CI was green over a
50×-hashrate bug.

The test now selects endianness per register, matching
`encode_data`:

```rust
let wire = if *big_endian { raw.to_be_bytes() } else { raw.to_le_bytes() };
```

**3. Every diagnostic looked healthy.** Chips enumerated 77/77,
ACKed every register write, ramped their PLL without protest, and
returned real nonces that hashed correctly against real block
headers. Job correlation and merkle computation were verified clean
across 21,500+ samples. Nothing pointed at cores.

**4. The reported hashrate was a hardcoded constant.** Per-thread
hashrate is `83 GH/s × chip_count`, set once in the constructor and
never updated from real nonce data. It reads **6.39 TH/s per board**
whether the board is doing 2 TH/s or 85 TH/s. Anyone glancing at the
API would think it was fine.

## Why it looked like a frequency problem

The symptom that misled several rounds: **nothing above ~300 MHz
appeared to hash.**

With only a handful of cores enabled per chip, throughput was so low
that nonce production at higher frequencies fell below the noise
floor of how it was being observed — while ~300 MHz happened to still
produce *just* enough nonces to look like it was working. That
created a false frequency ceiling, and sent the investigation into
PLL dividers, VCO stability, ramp granularity, and settle timing.
None of them were the problem.

The frequency ceiling was never real. After the fix, 575 MHz works
immediately.

## The thermal confirmation

Independent evidence that the mechanism is understood correctly:

| | Before fix | After fix |
|---|---|---|
| Hashrate | ~2 TH/s | ~105 TH/s |
| Fans | 50% | 100% |
| Board temp | **75 °C — tripped overtemp** | **49 °C, flat** |

A 40× hashrate increase cannot produce *less* heat. The chips were
already drawing roughly the same power beforehand — clocked and
switching at 575 MHz, but doing almost no useful work with it.
Efficiency went from order ~1,000 J/TH to **~23 J/TH**, against a
nameplate of 120 TH/s @ 2760 W = 23.0 J/TH.

Landing exactly at nameplate efficiency is itself evidence the fix is
complete rather than partial.

## How it was actually found

Worth recording, because the *method* mattered more than any
individual insight.

Every hypothesis generated by reasoning from symptoms was wrong —
`NonceRange` partitioning, `TicketMask` capping, job-ID bit packing,
PLL lock failure. Two of them were "confirmed" against a measurement
that was itself broken (see [lessons.md](lessons.md)).

What worked was mechanical and unglamorous: **read the reference
implementation systematically against the real captured wire bytes,
register by register, and check that each stored value produces the
documented bytes under its own encoder.**

The discrepancy is obvious once you compare `bm1366()` against
`bm1370()` side by side. Nobody had done that comparison, because the
symptoms all pointed somewhere else.
