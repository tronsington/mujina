#!/usr/bin/env python3
"""
Aggregating dashboard for the Antminer S19K Pro running Mujina.

Pulls three independent sources and keeps rolling history so you can
see the *effect* of a change rather than guessing from a snapshot:

  1. mujina's own REST API on the miner   -> temps, fans, PSU volts,
                                             threads, shares, difficulty
  2. the mining pool's API                -> authoritative accepted
                                             hashrate (5-min rate)
  3. HashScope (stratum MITM proxy)       -> per-submit accept/reject

Stdlib only, no external deps. Serves:

  GET /             the dashboard page
  GET /api/state    current snapshot + history as JSON

Why an aggregator rather than mujina's built-in dashboard: that one is
served *by* mujina, so it vanishes whenever bosminer is running, shows
no pool-side data, and keeps no history.
"""

import json
import os
import re
import threading
import time
import urllib.parse
import urllib.request
from collections import deque
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

# ---------------------------------------------------------------- config

# Via the local tunnel (see tunnel.sh): the miner's firewall is INPUT
# policy DROP with an allowlist that excludes 7785, so the API is not
# reachable across the LAN even though it binds 0.0.0.0.
# Everything below can be overridden from the environment; the
# defaults are what this was developed against.
MINER_BASE = os.environ.get("MINER_API_BASE", "http://127.0.0.1:7785")
MINER_API = MINER_BASE + "/api/v0/miner"
HASHSCOPE = os.environ.get("HASHSCOPE_BASE", "http://localhost:8000")

# The 256 Foundation pool. Its API is Prometheus-style: see
# `pool_rate()` below for the exact queries.
POOL_API = os.environ.get("POOL_API", "https://pool.256foundation.org/api/v1")

# Your payout identity at the pool, and the worker names to chart.
# NPUB is required for any pool-side figure -- without it the miner and
# HashScope panels still work, and the pool panel reads as unavailable.
NPUB = os.environ.get("POOL_NPUB", "")
WORKER = os.environ.get("POOL_WORKER", "mujina_s19k_pro")
BOSMINER_WORKER = os.environ.get("POOL_WORKER_BOSMINER", "braiins_s19k_pro")

LISTEN = (os.environ.get("LISTEN_HOST", "0.0.0.0"),
          int(os.environ.get("LISTEN_PORT", "8080")))

MINER_POLL_S = 3
HASHSCOPE_POLL_S = 15
POOL_POLL_S = 30

HISTORY_MAX = 2400  # ~2h at 3s

# APW12 exposes no current or power reading (only voltage setpoint,
# measured voltage, and on/off state), so wattage cannot be measured.
# This is the nameplate efficiency used to *estimate* it:
# 120 TH/s @ 2760 W = 23.0 J/TH, scaled by (V/13.9)^2.
NAMEPLATE_J_PER_TH = 23.0
NOMINAL_VOLTS = 13.9

# Each reported nonce represents 2^34 hashes at TicketMask zero_bits=2.
HASHES_PER_NONCE = 2 ** 34

# ---------------------------------------------------------------- state

_lock = threading.Lock()
_state = {
    "miner": None,
    "miner_error": "not polled yet",
    "pool": {},
    "hashscope": {},
    "history": deque(maxlen=HISTORY_MAX),
    "started": time.time(),
}


def _get_json(url, timeout=8):
    req = urllib.request.Request(url, headers={"Accept": "application/json"})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.loads(r.read().decode("utf-8", "replace"))


def _pool_query(promql, timeout=12):
    url = f"{POOL_API}/query?" + urllib.parse.urlencode({"query": promql})
    return _get_json(url, timeout=timeout)


# ------------------------------------------------------------ collectors


def poll_miner():
    """mujina's /miner endpoint. Absence here means mujina isn't running."""
    while True:
        try:
            d = _get_json(MINER_API, timeout=5)
            board = (d.get("boards") or [{}])[0]

            temps = [
                t["temperature_c"]
                for t in (board.get("temperatures") or [])
                if t.get("temperature_c") is not None
            ]
            fans = [
                f["rpm"] for f in (board.get("fans") or []) if f.get("rpm") is not None
            ]
            volts = [
                p["voltage_v"]
                for p in (board.get("powers") or [])
                if p.get("voltage_v") is not None
            ]

            sources = d.get("sources") or []
            difficulty = next(
                (s.get("difficulty") for s in sources if s.get("difficulty")), None
            )

            snap = {
                "t": time.time(),
                "uptime": d.get("uptime_secs"),
                # NOTE: this is scheduler measured_hashrate(); it reads 0
                # until the estimator settles, so it is NOT reliable early.
                "hashrate_reported": d.get("hashrate") or 0,
                "shares": d.get("shares_submitted") or 0,
                "best_difficulty": d.get("best_difficulty"),
                "paused": d.get("paused"),
                "difficulty": difficulty,
                "temps": temps,
                "temp_max": max(temps) if temps else None,
                "fans": fans,
                "volts": volts[0] if volts else None,
                "threads": [
                    {
                        "name": th.get("name"),
                        "active": th.get("is_active"),
                        # per-thread hashrate is the fixed nominal constant
                        # (83 GH/s * chip_count) -- kept for reference only
                        "nominal": th.get("hashrate"),
                    }
                    for th in (board.get("threads") or [])
                ],
                "board_name": board.get("name"),
                "board_model": board.get("model"),
                "serial": board.get("serial"),
                "temp_names": [
                    t.get("name") for t in (board.get("temperatures") or [])
                ],
                "sources": sources,
            }

            with _lock:
                prev = _state["history"][-1] if _state["history"] else None
                # Real measured hashrate from share rate:
                #   H = shares/s * difficulty * 2^32
                # Independent of any nominal constant in the firmware.
                snap["hashrate_shares"] = None
                if prev and difficulty:
                    dt = snap["t"] - prev["t"]
                    dshares = snap["shares"] - (prev.get("shares") or 0)
                    if dt > 0 and dshares >= 0:
                        snap["hashrate_shares"] = (
                            dshares / dt * difficulty * (2 ** 32)
                        )
                est_w = None
                if snap["volts"] and snap.get("hashrate_shares"):
                    th_s = snap["hashrate_shares"] / 1e12
                    jth = NAMEPLATE_J_PER_TH * (snap["volts"] / NOMINAL_VOLTS) ** 2
                    est_w = th_s * jth
                snap["est_watts"] = est_w
                _state["miner"] = snap
                _state["miner_error"] = None
                _state["history"].append(snap)
        except Exception as e:
            with _lock:
                _state["miner_error"] = f"{type(e).__name__}: {e}"
                _state["miner"] = None
        time.sleep(MINER_POLL_S)


def poll_pool():
    """Pool-side truth: accepted-share rate, last share, best ever."""
    if not NPUB:
        # Every query below is scoped by btcaddress, so without an
        # identity there is nothing to ask for. Say so once and leave
        # the panel empty rather than querying with a blank address.
        with _lock:
            _state["pool"] = {"error": "POOL_NPUB is not set"}
        print("dashboard: POOL_NPUB unset -- pool-side figures disabled",
              flush=True)
        return
    while True:
        out = {}
        try:
            q = (
                f'sum by(workername)(rate(worker_shares_valid_total'
                f'{{btcaddress="{NPUB}"}}[5m]))'
            )
            d = _pool_query(q)
            rates = {}
            for r in d.get("data", {}).get("result", []):
                rates[r["metric"].get("workername")] = float(r["value"][1])
            out["rate_5m"] = rates.get(WORKER)
            out["rate_5m_bosminer"] = rates.get(BOSMINER_WORKER)
            out["all_workers"] = rates
        except Exception as e:
            out["error"] = f"{type(e).__name__}: {e}"
        try:
            d = _pool_query(
                f'worker_last_share_at{{btcaddress="{NPUB}",workername="{WORKER}"}}'
            )
            res = d.get("data", {}).get("result", [])
            if res:
                out["last_share_at"] = float(res[0]["value"][1])
        except Exception:
            pass
        try:
            d = _pool_query(
                f'worker_best_share_ever{{btcaddress="{NPUB}",workername="{WORKER}"}}'
            )
            res = d.get("data", {}).get("result", [])
            if res:
                out["best_ever"] = float(res[0]["value"][1])
        except Exception:
            pass
        out["t"] = time.time()
        with _lock:
            _state["pool"] = out
        time.sleep(POOL_POLL_S)


def _msg_seq(msg_id):
    """HashScope ids are '<session-uuid>-<seq>'; return seq as int."""
    try:
        return int(str(msg_id).rsplit("-", 1)[1])
    except (IndexError, ValueError):
        return None


# Cumulative tally, because /api/messages only returns a trailing
# window (limit=N). Counting submits inside that window is NOT a total:
# as new mining.notify messages arrive they evict older submits, so the
# figure silently drifts *down*. We instead count each submit exactly
# once, keyed by its message sequence number.
_hs = {
    "sid": None,       # session this tally belongs to
    "counted": set(),  # submit seqs already tallied
    "accepted": 0,
    "rejected": 0,
    "lat_sum": 0.0,
    "lat_n": 0,
    "lat_max": 0.0,
    "recent": deque(maxlen=120),  # bools: True = rejected
    "partial": False,  # tally seeded mid-session, so totals are a floor
}


def poll_hashscope():
    """Per-submit accept/reject from the stratum MITM proxy."""
    while True:
        out = {}
        try:
            sessions = _get_json(f"{HASHSCOPE}/api/sessions", timeout=8)
            mine = [
                s for s in sessions if "mujina" in (s.get("user_agent") or "").lower()
            ]
            out["sessions_total"] = len(sessions)
            if mine:
                s = sorted(mine, key=lambda x: x.get("last_seen") or "")[-1]
                sid = s["session_id"]
                out["session"] = sid[:8]
                out["difficulty"] = s.get("difficulty")
                out["pool_peer"] = s.get("pool_peer")
                out["msg_count"] = s.get("message_count")

                LIMIT = 400
                msgs = _get_json(
                    f"{HASHSCOPE}/api/messages?session_id={sid}&limit={LIMIT}",
                    timeout=12,
                )

                # New session -> start a fresh tally. If it already has
                # more messages than one window holds, we cannot see the
                # earliest ones, so flag the total as a floor.
                if _hs["sid"] != sid:
                    _hs.update(
                        sid=sid, counted=set(), accepted=0, rejected=0,
                        lat_sum=0.0, lat_n=0, lat_max=0.0,
                        partial=(s.get("message_count") or 0) > LIMIT,
                    )
                    _hs["recent"].clear()

                subs = [
                    m for m in msgs
                    if (m.get("decoded") or {}).get("method") == "mining.submit"
                ]
                subs.sort(key=lambda m: _msg_seq(m.get("id")) or 0)
                for m in subs:
                    seq = _msg_seq(m.get("id"))
                    if seq is None or seq in _hs["counted"]:
                        continue
                    result = (m.get("response") or {}).get("result")
                    if result is None:
                        continue  # response not in yet; count on a later poll
                    _hs["counted"].add(seq)
                    rejected = result is False
                    if rejected:
                        _hs["rejected"] += 1
                    else:
                        _hs["accepted"] += 1
                    _hs["recent"].append(rejected)
                    lat = m.get("latency_ms")
                    if lat:
                        _hs["lat_sum"] += lat
                        _hs["lat_n"] += 1
                        _hs["lat_max"] = max(_hs["lat_max"], lat)

                # keep the dedupe set bounded
                if len(_hs["counted"]) > 20000:
                    cutoff = max(_hs["counted"]) - 10000
                    _hs["counted"] = {x for x in _hs["counted"] if x > cutoff}

                tot = _hs["accepted"] + _hs["rejected"]
                out["accepted"] = _hs["accepted"]
                out["rejected"] = _hs["rejected"]
                out["submits"] = tot
                out["partial"] = _hs["partial"]
                out["reject_pct"] = (100.0 * _hs["rejected"] / tot) if tot else None
                if _hs["lat_n"]:
                    out["latency_ms_avg"] = _hs["lat_sum"] / _hs["lat_n"]
                    out["latency_ms_max"] = _hs["lat_max"]
                # trailing-window reject rate: separates a settled
                # vardiff transient from an ongoing fault
                rec = list(_hs["recent"])
                if rec:
                    out["reject_pct_recent"] = 100.0 * sum(rec) / len(rec)
        except Exception as e:
            out["error"] = f"{type(e).__name__}: {e}"
        out["t"] = time.time()
        with _lock:
            _state["hashscope"] = out
        time.sleep(HASHSCOPE_POLL_S)


# ------------------------------------------------------------ http layer

HERE = Path(__file__).resolve().parent

# Static pages, all driven by the same /api/state payload.
PAGES = {
    "/": "dashboard.html",
    "/index.html": "dashboard.html",
    "/mmbn": "mmbn.html",
}


def windowed_hashrate(hist, end_idx=None, window=180):
    """Share-derived hashrate over a trailing window.

    Consecutive 3s samples are useless for this: at ~1 share per 6s
    most windows contain zero new shares, so the instantaneous figure
    flickers between 0 and nonsense. Summing per-segment work over a
    longer window fixes that, and summing segment-by-segment (rather
    than applying one difficulty to the whole span) stays correct
    across a vardiff change mid-window.

        work = sum(delta_shares_i * difficulty_i * 2^32)
        H    = work / elapsed
    """
    if end_idx is None:
        end_idx = len(hist) - 1
    if end_idx < 1:
        return None
    t_end = hist[end_idx]["t"]
    start = end_idx
    while start > 0 and t_end - hist[start - 1]["t"] < window:
        start -= 1
    if start == end_idx:
        return None

    work = 0.0
    for i in range(start + 1, end_idx + 1):
        prev, cur = hist[i - 1], hist[i]
        diff = cur.get("difficulty") or prev.get("difficulty")
        if not diff:
            continue
        d = (cur.get("shares") or 0) - (prev.get("shares") or 0)
        if d > 0:
            work += d * diff * (2 ** 32)
    elapsed = t_end - hist[start]["t"]
    if elapsed <= 0:
        return None
    return work / elapsed


def build_state_json():
    with _lock:
        hist = list(_state["history"])
        miner = _state["miner"]
        err = _state["miner_error"]
        pool = dict(_state["pool"])
        hs = dict(_state["hashscope"])
        started = _state["started"]

    def series(key):
        return [
            [round(h["t"], 1), h.get(key)]
            for h in hist
            if h.get(key) is not None
        ]

    # Smoothed share-derived hashrate: current value plus a series
    # sampled every ~10th history point (each over its own trailing
    # window), which is what the chart plots.
    hr_now = windowed_hashrate(hist)
    hr_series = []
    for i in range(1, len(hist), 10):
        v = windowed_hashrate(hist, end_idx=i)
        if v:
            hr_series.append([round(hist[i]["t"], 1), v])
    if hr_now:
        hr_series.append([round(hist[-1]["t"], 1), hr_now])

    return {
        "now": time.time(),
        "dashboard_uptime": time.time() - started,
        "miner": miner,
        "miner_error": err,
        "pool": pool,
        "hashscope": hs,
        "hashrate_windowed": hr_now,
        "series": {
            "hashrate_windowed": hr_series,
            "hashrate_shares": series("hashrate_shares"),
            "temp_max": series("temp_max"),
            "volts": series("volts"),
            "est_watts": series("est_watts"),
            "shares": series("shares"),
        },
        "temps_series": [
            [round(h["t"], 1), h.get("temps")] for h in hist[-400:] if h.get("temps")
        ],
        "fans_series": [
            [round(h["t"], 1), h.get("fans")] for h in hist[-400:] if h.get("fans")
        ],
        "pool_rate_series": [],
        "config": {
            "miner_api": MINER_API,
            "nameplate_j_per_th": NAMEPLATE_J_PER_TH,
            "nominal_volts": NOMINAL_VOLTS,
        },
    }


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass  # quiet

    def _send(self, body, ctype):
        if isinstance(body, str):
            body = body.encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def _proxy(self, upstream, ctype_default):
        """Relay a miner-side URL so it is reachable without opening
        port 7785 in the miner's firewall."""
        try:
            req = urllib.request.Request(upstream)
            with urllib.request.urlopen(req, timeout=8) as r:
                body = r.read()
                ctype = r.headers.get("Content-Type", ctype_default)
            self._send(body, ctype)
        except Exception as e:
            self._send(
                f"upstream {upstream} unreachable: {type(e).__name__}: {e}",
                "text/plain; charset=utf-8",
            )

    def do_GET(self):
        path = urllib.parse.urlparse(self.path).path
        if path == "/api/state":
            self._send(json.dumps(build_state_json()), "application/json")
        elif path == "/builtin":
            # mujina's own dashboard, relayed. Its JS fetches
            # /api/v0/miner relative to this origin, which the next
            # branch serves.
            self._proxy(MINER_BASE + "/dashboard", "text/html; charset=utf-8")
        elif path.startswith("/api/v0/"):
            self._proxy(MINER_BASE + path, "application/json")
        elif path in PAGES:
            fname = PAGES[path]
            try:
                html = (HERE / fname).read_text()
            except OSError as e:
                self._send(f"{fname} missing: {e}", "text/plain")
                return
            self._send(html, "text/html; charset=utf-8")
        else:
            self.send_response(404)
            self.send_header("Content-Length", "0")
            self.end_headers()


def main():
    for fn in (poll_miner, poll_pool, poll_hashscope):
        threading.Thread(target=fn, daemon=True).start()
    srv = ThreadingHTTPServer(LISTEN, Handler)
    print(f"dashboard on http://{LISTEN[0]}:{LISTEN[1]}/  (miner: {MINER_API})",
          flush=True)
    srv.serve_forever()


if __name__ == "__main__":
    main()
