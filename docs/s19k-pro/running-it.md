# Building, running, and measuring on the S19K Pro

## Cross-compilation

The control board runs a 4.9.113 aarch64 vendor kernel executing
**32-bit ARM EABI** userspace binaries. Target
`armv7-unknown-linux-musleabihf`, built with `zig cc` as the
cross-linker:

```sh
source "$HOME/.cargo/env"
PATH="$HOME/bin:$PATH" \
CC_armv7_unknown_linux_musleabihf=zigcc-armv7-musleabihf \
AR_armv7_unknown_linux_musleabihf=zigar-armv7-musleabihf \
CARGO_TARGET_ARMV7_UNKNOWN_LINUX_MUSLEABIHF_LINKER=zigcc-armv7-musleabihf \
RUSTFLAGS="-C link-self-contained=no" \
cargo build --release --target armv7-unknown-linux-musleabihf --bin mujina-minerd
```

Deploy with `scp -O` (the miner's SSH server is old enough that the
newer SFTP-based transfer fails).

> `cargo test` does **not** work on an x86_64 host here — the `udev`
> dependency needs `libudev` headers, and only the musl cross-target
> excludes it. Validate register byte order by arithmetic or on the
> target, not via host tests.

### Watch the miner's `/tmp`

`/tmp` on the miner is a **70 MB tmpfs**. Trace-level logs fill it in
minutes and a full `/tmp` produces confusing downstream failures.
Also: killed processes holding deleted log files keep the space
allocated — check with

```sh
for pid in /proc/[0-9]*; do for fd in $pid/fd/*; do
  t=$(readlink "$fd" 2>/dev/null); case "$t" in "/tmp/"*"(deleted)") echo "$pid $t";; esac
done; done
```

## Running

Stop the stock daemon first — it holds the UARTs and PSU:

```sh
/etc/init.d/S99bosminer stop     # takes ~20s
```

Then:

```sh
MUJINA_CONFIG=/tmp/mujina-s19k-real.toml \
MUJINA_POOL_URL='stratum+tcp://<pool>:3333' \
MUJINA_POOL_USER='<worker>' \
RUST_LOG='info,mujina_miner::asic::bm13xx=debug' \
/tmp/mujina-minerd
```

Board config: [reference/mujina-s19k-real.toml](reference/mujina-s19k-real.toml).
The mapping that matters for this unit — chains 0/1/2 →
`/dev/ttyS1`/`ttyS2`/`ttyS3`, `reset_gpio` 454/455/456, `detect_gpio`
439/440/441, PSU `enable_gpio` 437, temp/EEPROM on `/dev/i2c-1`.

**Set `default_fan_percent = 100`.** There is no dynamic fan control;
the value is applied once at startup and never revisited. At 50% the
board hits the overtemp cutoff in about four minutes under load.

### Shutdown

The binary does not reliably respond to `SIGTERM`; `SIGKILL` is often
required. `SIGKILL` skips the PSU `Drop` handler, so **verify the PSU
is actually off afterward** and disable it by hand if not:

```sh
cat /sys/class/gpio/gpio437/value    # 1 = disabled, 0 = enabled
echo 1 > /sys/class/gpio/gpio437/value
```

Note the miner's BusyBox has no `pgrep`/`pkill -f` reliability, no
`curl`, `wget`, `python`, `bash`, or `nc`. Kill by explicit PID and
verify via `/proc/<pid>`.

## Measuring hashrate correctly

This is the part most likely to mislead you.

### What is *not* a measurement

- **Per-thread `hashrate` from the API** is the fixed nominal
  constant `83 GH/s × chip_count` (6.39 TH/s per board), set once in
  the constructor and never updated from real nonces. It reads
  identically at 2 TH/s and at 85 TH/s.
- **`verify_chain` chip counts** undercount. It reported ~206/231
  chips alive while the board sustained a hashrate implying more were
  working. Treat post-ramp chip loss as a polling artifact unless
  corroborated.

### What is a measurement

**1. Nonce rate arithmetic.** With `TicketMask` at `zero_bits = 2`,
each reported nonce represents 2³⁴ ≈ 1.718 × 10¹⁰ hashes:

```
TH/s ≈ nonces_per_sec × 0.01718
```

Count `hash_diff=` lines at `RUST_LOG=info,mujina_miner::asic::bm13xx=debug`
(every processed nonce logs one, via either "Share found and sent" or
"Nonce does not meet target").

**2. The pool's own rate** — authoritative, since it counts only
accepted work. For a Prometheus-style pool API:

```sh
curl -sG "https://<pool>/api/v1/query" --data-urlencode \
  'query=sum by(workername)(rate(worker_shares_valid_total{btcaddress="<npub>"}[5m]))'
```

Expect ±10% swing on a 5-minute window — that's Poisson noise on the
share process, not instability.

**3. Share rate × difficulty**, as an independent cross-check:

```
TH/s = shares_per_sec × difficulty × 2³² / 1e12
```

All three agreed within noise at ~105–115 TH/s, which is why the
result is trustworthy.

### A MITM proxy is worth the setup

[HashScope](https://github.com/256foundation/HashScope) sits between
miner and pool and decodes every Stratum message. It's how the
before/after numbers here were established, and it lets you compare
`bosminer` and Mujina **as separate sessions against the same pool**.

```sh
cp env.example .env    # set POOL_HOST=stratum+tcp://<pool>, POOL_PORT=3333
docker compose up -d backend      # frontend optional; the REST API is enough
```

Point the miner at `stratum+tcp://<dev-host>:3333`. Then:

- `GET /api/sessions` — per-session assigned difficulty, message
  counts, user agent
- `GET /api/messages?session_id=...` — decoded `mining.submit` /
  `mining.notify` with paired responses and latency

This is how the reject rate was diagnosed (see below).

## Interpreting rejects

A burst of rejects right after hashrate changes is **normal**, not a
bug. When real throughput appears, the pool ratchets difficulty up
via vardiff; shares computed against the older, lower target arrive
after the pool has moved and get a bare
`{"result": false, "error": null}`.

Observed here: 199 submits split chronologically showed **32.3%
reject in the earlier half, 0.0% in the later half** as vardiff
settled (8,927 → 71,211 → 166,909). Submit latency was healthy
throughout at 39–47 ms, comparable to `bosminer`'s ~38 ms on the same
proxy.

Split your rejects by time before concluding anything.

## Applying the fix to the reference port

The ~105 TH/s result was demonstrated on
[Schnitzel/mujina](https://github.com/Schnitzel/mujina)'s
`amlogic-s19kpro-support` branch, which needs three things beyond a
plain build:

1. **A sibling dependency on an unmerged branch.**
   `Cargo.toml` references `amlogic-cb-tools` by relative path, and
   the `pic` module the board driver imports lives on the
   **`pic-microcontroller-driver`** branch, not `main`.
2. **A bit-bang I2C PSU shim.** The reference opens the PSU as a real
   `/dev/i2c-N` chardev; on this unit that fails with `ENXIO`. The
   PSU is only reachable over bit-banged GPIO **476/477**
   (`I2C_SCL`/`I2C_SDA` in Bitmain's own `/etc/init.d/S37board_setup`).
   The shim is in the patch.
3. **`default_fan_percent = 100`**, as above.

```sh
git clone --branch amlogic-s19kpro-support --single-branch \
  https://github.com/Schnitzel/mujina.git schnitzel-mujina
git clone --branch pic-microcontroller-driver --single-branch \
  https://github.com/Schnitzel/amlogic-cb-tools.git amlogic-cb-tools
cd schnitzel-mujina && git apply ../s19k-fixes.patch
```

Patch: [reference/s19k-fixes.patch](reference/s19k-fixes.patch) —
the `Core` byte-order fix and its corrected unit test, the
`NonceRange` revert, and the PSU shim.

Frequency is **compile-time only**: `target_frequency_mhz:
Some(575.0)` in `chip_config.rs`'s `bm1366()`. There is no config
key, environment variable, or API route for it — the v0 API's only
mutation is `PATCH /miner` with `paused`. Each frequency point needs
a rebuild.
