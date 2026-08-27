# Attribution and licensing

This S19K Pro work stands on other people's reverse engineering. This
file records what came from where, under what licence, and how it was
used, so that anyone redistributing this repository can meet its
obligations without re-deriving the history.

Mujina, and therefore everything here, is **GPL-3.0-or-later**.

## Code derived from other projects

Two places carry other people's work rather than merely citing it.
Both are GPL-3.0, the same licence family as this repository, so no
notice-preservation asymmetry applies --- but both must keep their
attribution.

### `amlogic-cb-tools` → `mujina-miner/src/peripheral/apw12.rs`

- Upstream: <https://github.com/skot/amlogic-cb-tools> (also
  <https://github.com/Schnitzel/amlogic-cb-tools>)
- Licence: **GPL-3.0**
- What came across: the APW12 PSU protocol, and the DAC/ADC
  calibration constants, reverse-engineered in that project's
  `apw12-psu-tool`. Our `apw12.rs` is a port of that protocol, and its
  module header says so.

> **A licence-metadata trap.** That project's `Cargo.toml` declares
> `license = "MIT"`, but its `LICENSE` file is the GPL-3.0 text and its
> README states GPLv3. Two of three say GPL-3.0 and the `LICENSE` file
> is the operative one, so it is treated as GPL-3.0 here. It makes no
> practical difference to us --- MIT and GPL-3.0 are both compatible
> with redistributing under GPL-3.0-or-later --- but do not repeat the
> `Cargo.toml` claim without checking, and if you lift more code from
> that project, resolve the ambiguity with its authors first.

`amlogic-cb-tools` is **not** a build dependency of this repository.
The reference port needs it as a sibling checkout for its `pic` module;
this repo does not, because the equivalent native GPIO/I2C support is
in-tree under `mujina-miner/src/linux_hw/`. The PIC handshake it
provides is for PIC-variant hashboards (BHB42601, S19j Pro family,
BM1362) and is not needed by the BHB56902/BM1366 boards here.

### `Schnitzel/mujina` → `reference/s19k-fixes.patch`

- Upstream: <https://github.com/Schnitzel/mujina>, branch
  `amlogic-s19kpro-support`
- Licence: **GPL-3.0-or-later**
- What it is: a patch *against their code*, not ours. It carries the
  `Core` byte-order fix and its corrected unit test, the `NonceRange`
  revert, and a bit-banged-GPIO PSU shim. The file's own header records
  the base commit it applies to.

That fork is where **~105-115 TH/s was demonstrated**. Any claim in
these docs about 575 MHz throughput belongs to it, not to this
repository's driver. See [README.md](README.md), "Two codebases, and
which one is proven".

## Referenced, not incorporated

Cited by URL in the docs or source; no code taken.

| Project | Licence | Used for |
|---|---|---|
| [256foundation/mujina](https://github.com/256foundation/mujina) | GPL-3.0-or-later | Upstream. This repository is a fork. |
| [256foundation/HashScope](https://github.com/256foundation/HashScope) | MIT | Stratum MITM proxy. Every before/after hashrate and reject-rate figure in these docs was established with it. Run from its own repository --- deliberately not vendored. |
| [skot/BM1397](https://github.com/skot/BM1397) | see repo | BM13xx register and protocol documentation, the basis for much of `asic/bm13xx/REFERENCE.md`. |
| [bitaxeorg/ESP-Miner](https://github.com/bitaxeorg/ESP-Miner) | see repo | BM1366 init sequences; specific PRs are cited inline where a value came from one. |
| [HashSource/Antminer-APW12-Firmware](https://github.com/HashSource/Antminer-APW12-Firmware) | see repo | APW12 firmware behaviour, cross-checked against observed PSU responses. |
| [ziglang/zig](https://ziglang.org) | MIT | `zig cc` as the armv7 musl cross compiler and linker. Fetched by `tools/cross/fetch-zig.sh`, not vendored. |

Component datasheets (TI TPS546D24A, TMP451, TMP1075; Microchip
EMC2101; the PMBus specification) are cited inline in
[hardware.md](hardware.md) where a register or constant came from one.

## If you add to this

- Adding a **dependency**: `deny.toml` enforces the licence allow-list;
  `just deny` will tell you if a licence is not on it. Add it there,
  with a comment saying why it is acceptable, rather than working
  around the check.
- Lifting **code** from another project: add it to the first section
  above, name the file it landed in, and put the provenance in that
  file's module header too --- the way `apw12.rs` does. A reader in the
  source should not have to find this file to learn that code is
  someone else's.
- Citing a **document, capture, or datasheet**: a URL at the point of
  use is enough; add it to the table only if it becomes load-bearing.
