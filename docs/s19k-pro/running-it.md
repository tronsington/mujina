# Building, running, and measuring on the S19K Pro

## Cross-compilation

The control board runs a 4.9.113 aarch64 vendor kernel that executes
**32-bit ARM EABI** userspace binaries, so the build target is
`armv7-unknown-linux-musleabihf`, not an aarch64 one.

`zig cc` stands in for a musl cross-toolchain: a single self-contained
download, no root, no distro package, and no separately built sysroot.
Fetch it once, then build:

```sh
just fetch-zig      # pinned zig 0.16.0, checksum-verified
just build-s19k     # -> target/armv7-unknown-linux-musleabihf/release/mujina-minerd
```

`build-s19k` adds the rustup target if it is missing, and
`just deploy-s19k root@<miner>` copies the result to the miner's `/tmp`
with `scp -O` --- the miner's SSH server is old enough that the newer
SFTP-based transfer fails.

Plain cargo works too. The compiler, linker and archiver wiring lives
in `.cargo/config.toml`, so nothing needs to be set in the
environment:

```sh
cargo build --release --target armv7-unknown-linux-musleabihf \
    --bin mujina-minerd
```

To build the diagnostic tools as well, add `--features dev-tools`; see
[hardware.md](hardware.md) for what each one is for.

Two consequences of this target worth knowing, both already handled in
the manifests:

- **No OpenSSL.** `openssl-sys` does not cross-compile here, so
  `reqwest` is configured for `rustls` instead of its default
  `native-tls`. That is why `deny.toml` allows the ISC and
  CDLA-Permissive-2.0 licenses the rustls chain carries.
- **No `udev`.** `tracing-journald` and `udev` are gated to
  `target_env = "gnu"`, so the musl target excludes them --- which is
  also why host tests behave differently (see below).

> `cargo test` does **not** work on an x86_64 host without `libudev`
> headers installed; only the musl cross-target excludes `udev`. Use
> `just in-container test`, which runs in the build container where
> those headers are present, or validate register byte order by
> arithmetic or on the target.

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

Two different binaries get run on this hardware, and they are
configured differently. Confusing them is the most common way to get a
daemon that starts cleanly and never touches the hashboards.

- **This repository's `mujina-minerd`** — what the rest of this
  document means by "Mujina". Configured entirely by environment
  variables.
- **The reference port's binary** — [Schnitzel/mujina](https://github.com/Schnitzel/mujina)'s
  `amlogic-s19kpro-support` branch, which is where ~105 TH/s was
  demonstrated. Configured by a TOML file. See
  [Reproducing the 105 TH/s result](#reproducing-the-105-ths-result)
  at the end.

Either way, stop the stock daemon first — it holds the UARTs and PSU:

```sh
/etc/init.d/S99bosminer stop     # takes ~20s
```

### Running this repository's daemon

```sh
MUJINA_ANTMINER_S19K_AM3_ENABLE=1 \
MUJINA_POOL_URL='stratum+tcp://pool.256foundation.org:3333' \
MUJINA_POOL_USER='<your-npub>.<worker>' \
MUJINA_API_LISTEN=0.0.0.0:7785 \
RUST_LOG='info,mujina_miner::asic::bm13xx=debug' \
/tmp/mujina-minerd
```

**`MUJINA_ANTMINER_S19K_AM3_ENABLE` is not optional.** This board is
not discovered from hardware — it *is* the host control board — so it
registers as a `VirtualBoardDescriptor` and stays inert unless that
variable is set. Without it the daemon starts, serves its API, connects
to the pool, and never drives a single chip.

`mujina-minerd --help` lists every control variable.

**What a healthy start looks like**, and how far it currently gets:

```
INFO daemon: Antminer S19K Pro (AM3) enabled
INFO board::antminer_s19k_am3: Hashboard presence detect presence=[true, true, true]
WARN board::antminer_s19k_am3: PSU set_voltage failed, retrying   <- normal, bit-banged bus
DEBUG asic::bm13xx::thread: Sweeping SetChipAddress
DEBUG asic::bm13xx::thread: Sending broadcast core configuration (CORE_MAILBOX, now correctly big-endian)
DEBUG asic::bm13xx::thread: Sending per-chip InitControl/MiscControl/CORE_MAILBOX pass
DEBUG asic::bm13xx::thread: Switching to real operating baud baud=3125000
   ... chain=1/2/3, chips=77 each
```

`GET /api/v0/miner` then reports six temperature sensors, PSU voltage,
and the pool source with its assigned difficulty.

**Expect it to stop there for now.** On a fresh build (checked
2026-08-27) the sequence completes, all 231 chips enumerate, the
frequency ramp runs to its 575 MHz target, the pool connects — and no
nonces follow, with board temperature flat at ambient.

This is the retest the earlier notes kept calling for, and the result
is negative: **the `Core` byte-order fix is present in this driver and
is not on its own enough to make it hash at 575 MHz.** Whatever else
the reference port does differently has not been identified. Round 12
did mine real accepted shares with this driver and Round 14 reached
~4-6 TH/s at ~300 MHz, so the path is not hypothetical — but it is not
reproducible from a fresh build today.

If you want the ~105 TH/s result now, use the reference port — see
[Reproducing the 105 TH/s result](#reproducing-the-105-ths-result).

A flat temperature is the diagnostic worth internalising: chips that
are powered but not hashing sit at ambient. Rising temperature is the
first real sign of work, well before any hashrate figure is
trustworthy.

> **`MUJINA_CONFIG` does nothing in this repository.** `Config::load`
> and `Config::load_from` are unimplemented upstream stubs and are
> never called, so passing a TOML file is silently ignored. That
> variable belongs to the reference port. This repository's board
> mapping lives in compile-time constants in
> `mujina-miner/src/board/antminer_s19k_am3.rs`: `CHAIN_TTYS`,
> `CHAIN_ENABLE_OFFSETS`, `CHAIN_PRESENCE_OFFSETS`, `PSU_NEN_OFFSET`,
> `PSU_SCL_GPIO`/`PSU_SDA_GPIO`, `PSU_TARGET_VOLTS`,
> `TEMP_SENSOR_ADDRS`. Changing the mapping means editing those and
> rebuilding.

The mapping those constants encode, for this unit: chains 1/2/3 →
`/dev/ttyS1`/`ttyS2`/`ttyS3`, enable GPIO 454/455/456, presence GPIO
439/440/441, PSU enable GPIO 437 (active low), PSU I2C bit-banged on
GPIO 476/477, temperature sensors on `/dev/i2c-1`. Full detail in
[hardware.md](hardware.md) and
[../../mujina-miner/src/board/antminer_s19k_am3.md](../../mujina-miner/src/board/antminer_s19k_am3.md).

### Fans: set them yourself before you start this daemon

> **This repository's driver does not touch the fans at all.** It has
> no fan control, no fan telemetry, and **no overtemp gate** — it reads
> the six board sensors for telemetry only and will not stop on
> temperature. The 75 °C cutoff described elsewhere in these docs is
> the *reference port's* software gate. Nothing in this binary protects
> the board.

That matters because the fans do not reliably keep spinning between
runs. `bosminer` and the reference port both zero the PWM duty on a
clean shutdown, so after stopping either one you can be left at
`duty_cycle = 0` — no airflow — and this daemon will happily power 231
chips into it.

**Check, and set to 100%, before every start:**

```sh
for c in /sys/class/pwm/pwmchip0/pwm0 /sys/class/pwm/pwmchip0/pwm1; do
    cat "$c/period" > "$c/duty_cycle"      # duty = period = 100%
    echo 1 > "$c/enable"
done
grep . /sys/class/pwm/pwmchip0/pwm*/duty_cycle   # expect 10000 twice
```

Two PWM channels drive four fans (0&1 on `pwm0`, 2&3 on `pwm1`), and
this daemon leaves whatever it finds untouched — so whatever you set
persists for the whole run, and after it exits.

**Never disable the fans** — not as a test, not briefly. At 50% this
board reaches the overtemp cutoff in about four minutes under load.


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

## Unattended operation

`mujina-minerd` has no supervisor of its own. If it exits, nothing
restarts it — and because an unclean exit skips the Rust `Drop`
handler, it leaves the **PSU still enabled** with the chips idle. That
is thermally harmless (fans hold their last PWM duty) but it silently
stops mining, which matters for overnight soaks and long tuning runs.

[`tools/miner-supervisor/mujina-supervisor.sh`](../../tools/miner-supervisor/mujina-supervisor.sh) is a
minimal supervisor for this. Set the env block at the top, then:

```sh
/sbin/start-stop-daemon -S -b -m \
  -p /tmp/mujina-supervisor.pid -x /tmp/mujina-supervisor.sh
```

It does three things a naive `while true` loop does not:

1. **Forces the PSU off between attempts.** Relaunching onto a hot rail
   skips the known-good cold-start path (12 V ramp + chain reset), and
   it means the window between crash and restart is spent with the rail
   *off* — the safest state, since mujina's own overtemp gate dies with
   the process.
2. **Backs off on a crash loop.** Runs shorter than 120 s count as
   failures, with a progressively longer delay. Hammering a 2 kW PSU
   on/off repeatedly is worse than not restarting at all.
3. **Falls back to `bosminer`** after five consecutive short runs, so
   the machine keeps mining even if mujina is unstable.

Logs to `/tmp/mujina-supervisor.log` (restart history) and
`/tmp/mujina.log` (current run, with `.prev` kept for the one before).
At `RUST_LOG=info` that runs around 3–4 MB per 8 h — fine against the
70 MB tmpfs, but worth watching if you raise the log level.

Verify it actually works before trusting it: kill mujina and confirm
the supervisor logs the exit and relaunches.

### Two traps on this hardware

- **`setsid` does not exist on the miner.** `setsid nohup ... &` fails
  silently — the command simply isn't there, so nothing starts and
  nothing is logged. Use `start-stop-daemon`, which BraiinsOS already
  uses for `bosminer` itself.
- **Beware self-matching process searches.** `pkill -f mujina-minerd`,
  or a `grep` over `/proc/*/cmdline`, will also match *your own shell*
  when the search string appears in its command line — killing your SSH
  session instead of the target. Match `argv[0]` exactly:

  ```sh
  for p in $(ls /proc | grep -E '^[0-9]+$'); do
    a=$(tr '\0' '\n' < /proc/$p/cmdline 2>/dev/null | head -1)
    [ "$a" = "/tmp/mujina-minerd-corefix" ] && echo $p
  done
  ```

Note the binary and this script both live in the miner's `/tmp`, which
is a tmpfs wiped on reboot — redeploy both after a miner restart.

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

## Reproducing the 105 TH/s result

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
