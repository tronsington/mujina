#!/bin/sh
# Keep mujina-minerd running, unattended.
#
# Two things this does that a naive `while true; do ...; done` does not:
#
#  1. Forces the PSU off between attempts. An unclean exit (SIGKILL, or
#     any abort that skips the Rust Drop handler) leaves gpio437 = 0,
#     i.e. the rail still enabled. Relaunching into a hot rail skips the
#     known-good cold-start path (12V ramp + chain reset). Cycling it
#     also means the gap between crash and restart is spent with the
#     PSU OFF, which is the safest state while mujina's own overtemp
#     gate is not running.
#
#  2. Backs off on a crash loop and falls back to bosminer. Hammering a
#     2 kW PSU on/off repeatedly is worse than not restarting at all.
#     A run shorter than MIN_GOOD_RUN counts as a failure; after
#     MAX_FAILS consecutive failures we stop and hand back to bosminer
#     so the machine keeps mining regardless.
#
# Fans are unaffected throughout: PWM duty persists in hardware, so they
# stay where they were (100%) even while mujina is down.

BIN=/tmp/mujina-minerd-corefix
PSU_GPIO=/sys/class/gpio/gpio437/value      # active-low: 1 = disabled
LOG=/tmp/mujina.log
SUPLOG=/tmp/mujina-supervisor.log
MIN_GOOD_RUN=120                            # seconds; shorter = a failure
MAX_FAILS=5

export MUJINA_CONFIG=/tmp/mujina-s19k-real.toml
export MUJINA_API_LISTEN=0.0.0.0:7785
export MUJINA_POOL_URL='stratum+tcp://<pool-or-proxy-host>:3333'
export MUJINA_POOL_USER='<your-worker>.mujina_s19k_pro'
export RUST_LOG='info,mujina_miner::asic::bm13xx=info'

log() { echo "$(date +%Y-%m-%dT%H:%M:%S) $*" >> "$SUPLOG"; }

log "supervisor starting (pid $$)"
fails=0

while true; do
    # clean slate: never bring up onto an already-hot rail
    echo 1 > "$PSU_GPIO" 2>/dev/null
    sleep 3

    [ -f "$LOG" ] && mv -f "$LOG" "$LOG.prev" 2>/dev/null   # keep 1 previous run

    start=$(date +%s)
    "$BIN" > "$LOG" 2>&1
    rc=$?
    ran=$(( $(date +%s) - start ))

    # an unclean exit may have left the rail enabled
    echo 1 > "$PSU_GPIO" 2>/dev/null

    log "mujina exited rc=$rc after ${ran}s"

    if [ "$ran" -lt "$MIN_GOOD_RUN" ]; then
        fails=$(( fails + 1 ))
    else
        fails=0
    fi

    if [ "$fails" -ge "$MAX_FAILS" ]; then
        log "$fails consecutive short runs — giving up on mujina"
        log "starting bosminer so the machine keeps mining"
        /etc/init.d/S99bosminer start >> "$SUPLOG" 2>&1
        log "supervisor exiting"
        exit 1
    fi

    delay=$(( 5 * fails + 5 ))
    log "restarting in ${delay}s (consecutive failures: $fails)"
    sleep "$delay"
done
