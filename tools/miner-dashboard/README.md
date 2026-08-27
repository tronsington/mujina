# Miner dashboard

A read-only aggregator that puts the three sources of truth about a
running miner on one page, so you can see the *effect* of a change
instead of guessing from a snapshot:

1. **Mujina's own REST API** on the miner — temperatures, fans, PSU
   voltage, threads, shares, difficulty.
2. **The pool's API** — the authoritative accepted-hashrate figure,
   since it counts only work the pool took.
3. **[HashScope](https://github.com/256foundation/HashScope)** — a
   Stratum MITM proxy, for per-submit accept/reject and latency.

Python standard library only — no dependencies, no build step.

## Why not Mujina's built-in dashboard

Mujina serves its own page, but it is served *by* Mujina: it vanishes
whenever the daemon is stopped (so you cannot use it to compare against
stock firmware), it shows no pool-side data, and it keeps no history.
This one proxies that page at `/builtin` so both are available.

## Running

```sh
# Terminal 1 --- keep the API reachable (see "The tunnel" below)
./tunnel.sh root@<miner-ip>

# Terminal 2
POOL_NPUB=npub1... ./server.py
```

Then open <http://localhost:8080/>.

| Path | What |
|---|---|
| `/` | The dashboard |
| `/mmbn` | The same data, themed |
| `/builtin` | Mujina's own page, proxied from the miner |
| `/api/state` | Current snapshot plus history, as JSON |

## Configuration

Everything is an environment variable; the defaults are what this was
developed against.

| Variable | Default | Notes |
|---|---|---|
| `POOL_NPUB` | *(none)* | Your payout identity at the pool. Without it the pool panel is disabled and the miner/HashScope panels still work. |
| `POOL_WORKER` | `mujina_s19k_pro` | Worker name to chart |
| `POOL_WORKER_BOSMINER` | `braiins_s19k_pro` | A second worker to chart alongside, for comparing against stock firmware |
| `POOL_API` | `https://pool.256foundation.org/api/v1` | Prometheus-style pool API |
| `MINER_API_BASE` | `http://127.0.0.1:7785` | Mujina's API, via the tunnel |
| `HASHSCOPE_BASE` | `http://localhost:8000` | Optional; the panel degrades if absent |
| `LISTEN_HOST` / `LISTEN_PORT` | `0.0.0.0` / `8080` | |
| `MINER_API_PORT` | `7785` | Used by `tunnel.sh` |

## The tunnel

The miner's firewall is `INPUT` policy `DROP` with an allowlist that
does not include 7785, so Mujina's API is not reachable across the LAN
even though it binds `0.0.0.0`. `tunnel.sh` forwards
`127.0.0.1:7785` to the same port on the miner and reconnects if the
link drops. Tunnelling is preferable to punching a hole for an
unauthenticated API.

## A caution about the numbers

**Per-thread `hashrate` from Mujina's API is not a measurement.** It is
a fixed nominal constant (chip count × 83 GH/s) set once in the
constructor, and it reads identically at 2 TH/s and at 85 TH/s. The
dashboard shows it only to confirm threads are alive, and derives its
headline hashrate from share rate × difficulty instead.

**Power is estimated, not measured.** The APW12 exposes voltage and
on/off state but no current, so wattage cannot be read in software. The
figure shown is nameplate efficiency (120 TH/s @ 2760 W = 23.0 J/TH)
scaled by (V/13.9)². Use a wall meter or PDU for a real number.

See [docs/s19k-pro/running-it.md](../../docs/s19k-pro/running-it.md),
"Measuring hashrate correctly", for the full account of what is and is
not a measurement on this hardware.
