# Antminer S19K Pro Board Support

This document describes mujina-miner's support for the Antminer S19K Pro
with the Amlogic A113D ("AM3") control board, replacing stock
BraiinsOS+ `bosminer`.

## Overview

The Antminer S19K Pro is a ~2 kW, 120 TH/s production miner: three
BHB56902 hashboards of 77 BM1366 ASICs each (231 total), one shared
APW12 power supply, four fans, all driven by an Amlogic A113D control
board running a 4.9.113 aarch64 vendor kernel.

Unlike the Bitaxe Gamma and EmberOne00, this board has **no USB
co-processor**. The control board *is* the host: it exposes GPIO, I2C
and UARTs directly from the SoC to Linux, so the driver talks to
hardware through sysfs and `/dev/i2c-N` rather than tunnelling a
management protocol over USB. It therefore registers as a
`VirtualBoardDescriptor` instead of matching a USB vendor/product ID,
and is enabled explicitly:

```sh
MUJINA_ANTMINER_S19K_AM3_ENABLE=1 mujina-minerd
```

Without that variable the driver never runs.

## Status

**Mining, with real accepted shares.** Confirmed stable at ~300 MHz for
roughly 4-6 TH/s.

The hardware is proven to reach **~105-115 TH/s at 575 MHz** with a 0%
reject rate at settled pool difficulty -- but that was measured against
an independently developed reference port, not this driver. The
byte-order fix that unlocked it is present here, and this driver has not
been retested above 300 MHz since. Any shortfall now is a driver bug,
not a hardware limit. See
[docs/s19k-pro/README.md](../../../docs/s19k-pro/README.md).

## Board Architecture

Three hashboard chains, each on its own UART, all reset together --- a
real hardware requirement: a single chain enabled alone never responds,
even at correct voltage.

| Chain | UART | Enable GPIO | Presence GPIO |
|---|---|---|---|
| 1 | `/dev/ttyS1` | 454 | 439 |
| 2 | `/dev/ttyS2` | 455 | 440 |
| 3 | `/dev/ttyS3` | 456 | 441 |

Discovery runs at 115200 baud, then optionally switches to `bosminer`'s
real 3,125,000 operating baud.

These values are compile-time constants in
[`antminer_s19k_am3.rs`](antminer_s19k_am3.rs), not configuration ---
`Config::load` is an unimplemented upstream stub, so `MUJINA_CONFIG`
does nothing in this repository.

## Hardware Components

- **BM1366 ASICs**: 77 per hashboard, 11 voltage domains in series per
  board, so per-domain voltage is rail / 11.
- **APW12 PSU**: enable on GPIO 437 (active low, `1` = disabled),
  target 13.9 V. Its I2C is **not** on any `/dev/i2c-N` device --- it
  is bit-banged over GPIO 476/477, labelled `I2C_SCL`/`I2C_SDA` in
  Bitmain's own `/etc/init.d/S37board_setup`.
- **6 × TMP75-compatible sensors** on `/dev/i2c-1`, an (inlet, outlet)
  pair per hashboard. Overtemp cutoff is 75 °C.
- **4 fan channels on 2 PWM channels**: fans 0&1 on `pwm0`, fans 2&3 on
  `pwm1`. There is no dynamic fan control --- `default_fan_percent` is
  applied once at startup and never revisited.

> **Never disable the fans.** Not as a test, not briefly. This machine
> moves real heat: at 50% fans it reached the overtemp cutoff in about
> four minutes under load. Use 100% for any sustained run.

Note that fan 3 reads 0 RPM on the development unit. Stock `bosminer`
read it as 0 as well, so it is a pre-existing hardware fault rather
than anything the driver does --- but it means that unit runs on three
of four fans, which matters when considering frequency headroom.

## References

- [docs/s19k-pro/](../../../docs/s19k-pro/) --- the full documentation
  set: hardware reference, bring-up log, root-cause analysis, build and
  run instructions, and lessons.
- [docs/s19k-pro/hardware.md](../../../docs/s19k-pro/hardware.md) ---
  GPIO map, UART chains, I2C buses, PSU protocol, flash layout, and how
  each was confirmed.
- [docs/s19k-pro/running-it.md](../../../docs/s19k-pro/running-it.md)
  --- cross-compiling, deploying, running, and measuring hashrate
  correctly on this hardware.
- [`asic/bm13xx/REFERENCE.md`](../asic/bm13xx/REFERENCE.md) --- BM13xx
  register and protocol reference.
