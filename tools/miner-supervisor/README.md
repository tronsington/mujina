# Miner supervisor

`mujina-minerd` has no supervisor of its own. If it exits, nothing
restarts it — and because an unclean exit skips the Rust `Drop`
handler, it leaves the **PSU still enabled** with the chips idle. That
is thermally harmless (fans hold their last PWM duty) but it silently
stops mining, which matters for overnight soaks and long tuning runs.

`mujina-supervisor.sh` is a minimal supervisor for exactly that. Set
the environment block at the top, copy it to the miner, and start it
with BusyBox's `start-stop-daemon`:

```sh
/sbin/start-stop-daemon -S -b -m \
  -p /tmp/mujina-supervisor.pid -x /tmp/mujina-supervisor.sh
```

It does three things a naive `while true` loop does not:

1. **Forces the PSU off between attempts.** Relaunching onto a hot rail
   skips the known-good cold-start path (12 V ramp + chain reset), and
   the window between crash and restart is spent with the rail *off* —
   the safest state, since Mujina's own overtemp gate dies with the
   process.
2. **Backs off on a crash loop.** Runs shorter than 120 s count as
   failures, with a progressively longer delay. Hammering a 2 kW PSU
   on/off repeatedly is worse than not restarting at all.
3. **Falls back to `bosminer`** after five consecutive short runs, so
   the machine keeps mining even if Mujina is unstable.

**Set the fans to 100% before starting this**, and verify it. This
repository's `mujina-minerd` never touches the PWM, so whatever duty is
set persists across restarts and after it exits — but `bosminer` and the
reference port both zero the duty on a clean shutdown, so you may be
starting from no airflow. The daemon has no overtemp gate of its own.
See [docs/s19k-pro/running-it.md](../../docs/s19k-pro/running-it.md),
"Fans: set them yourself".

## Two traps on this hardware

- **`setsid` does not exist on the miner.** `setsid nohup ... &` fails
  silently — the command simply is not there, so nothing starts and
  nothing is logged. Use `start-stop-daemon`, which BraiinsOS already
  uses for `bosminer` itself.
- **Beware self-matching process searches.** `pkill -f mujina-minerd`,
  or a `grep` over `/proc/*/cmdline`, also matches *your own shell*
  when the search string appears in its command line — killing your SSH
  session instead of the target. Match `argv[0]` exactly.

Both the supervisor and the binary live in the miner's `/tmp`, a tmpfs
wiped on reboot, so redeploy both after a miner restart.

Verify it actually works before trusting it: kill Mujina and confirm
the supervisor logs the exit and relaunches.

See [docs/s19k-pro/running-it.md](../../docs/s19k-pro/running-it.md),
"Unattended operation".
