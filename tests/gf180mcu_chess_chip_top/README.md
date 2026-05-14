# GF180MCU chess chip_top smoke test

This directory contains the smoke-test recipe for running the
**wafer.space chess chip_top** netlist (≈ 227,000 cells) through
Jacquard's GF180MCU pipeline end-to-end. It exists to validate that
the GF180MCU support (Phases 4 / 4b / 6) scales to a real wafer.space-
class design with a pad ring around a non-trivial digital core.

The post-P&R netlist itself is ~200 MB and not committed. Regenerate
it from the upstream project, or use a previously-built copy.

## Inputs

1. **Netlist:** `chip_top.nl.v` from a LibreLane run of the chess
   design. Source: <https://github.com/Ravenslofty/gf180mcu-chess>,
   branch `chess`.

   Build with `make librelane` from the chess repo root. The netlist
   lands at
   `librelane/runs/RUN_<timestamp>/43-openroad-detailedrouting/chip_top.nl.v`.
   The `make` may fail in a post-routing ECO step — the netlist is
   complete by then.

2. **Stimulus VCD:** generated from `gen_stim.v` in this directory.

## Cell families exercised

| Family | Count | Path through Jacquard |
|---|---|---|
| `gf180mcu_fd_sc_mcu9t5v0__*` std cells | ~225,800 | `gf180mcu_pdk` decompose + AIG |
| `gf180mcu_fd_io__bi_24t` | 40 | `aig::gf180mcu_postprocess` IO-pad branch |
| `gf180mcu_fd_io__in_c` | 13 | `aig::gf180mcu_postprocess` IO-pad branch |
| `gf180mcu_fd_io__in_s` | 1 | `aig::gf180mcu_postprocess` IO-pad branch |
| `gf180mcu_fd_io__cor` | 4 | filler-classified |
| `gf180mcu_fd_io__fill{10,5,1,nc}` | 1,062 | filler-classified |
| `gf180mcu_fd_io__asig_5p0` | 2 | filler-classified |
| `gf180mcu_ws_io__dvdd` / `dvss` | 18 | filler-classified |
| `gf180mcu_ws_ip__id` / `logo` | 2 | filler-classified |
| `dffrnq_*`, `dffsnq_*`, `dffq_*` | ~7,550 | `aig::gf180mcu_postprocess` sequential |

## Recipe

```sh
# 1. Stage the netlist somewhere outside this repo (it's ~200 MB).
mkdir -p /tmp/claude/chess-sim
cp <librelane-output>/43-openroad-detailedrouting/chip_top.nl.v \
   /tmp/claude/chess-sim/

# 2. Generate stim VCD (uses iverilog).
cd tests/gf180mcu_chess_chip_top
iverilog -o stim_gen gen_stim.v && ./stim_gen
mv stim.vcd /tmp/claude/chess-sim/

# 3. Run jacquard sim on Metal.
cargo run -r --features metal --bin jacquard -- sim \
    /tmp/claude/chess-sim/chip_top.nl.v \
    /tmp/claude/chess-sim/stim.vcd \
    /tmp/claude/chess-sim/out.vcd \
    1 \
    --top-module chip_top \
    --max-clock-edges 200
```

`--max-clock-edges 200` caps the run at 100 full cycles. The chess
core won't do anything observable without JTAG stim through
`bidir_PAD` — the smoke is "pipeline survives this scale".

## Expected output

On an Apple M4 Pro:

* Cell library detection: GF180MCU
* Clock tracing: ~40 ms (7,550 sequential cells)
* AIG DFS build: ~180 ms (~267k AIG pins)
* Partitioning + merging: ~19 s → 20 partitions
* Script build: ~1 s (~1.39M instructions)
* Metal kernel: instant for 106 cycles
* Total wall-clock: ≈ 20 s

The output VCD will be small (~1 kB): chip_top has no
`Direction::I` top-level ports — every primary signal is an `inout`
pad — so Jacquard finds no signal to emit per-cycle samples for.
The cycle timestamps are emitted as bare `#N` records. Adding
visible outputs requires hierarchical hooks into core internals
(`bidir_PAD2CORE[i]` and similar), which is out of scope for this
smoke test.

## What this catches

* GF180MCU prefix detection (`gf180mcu_fd_*` and `gf180mcu_ws_*`).
* IO-pad pin-table coverage for `bi_24t` / `in_c` / `in_s` / `asig_5p0`.
* Filler classification for IO-ring fillers, corner, analog
  passthrough, wafer.space power pads, ws_ip empty stubs.
* Clock-path tracer handling of `in_c` / `in_s` (pad-as-buffer).
* Tie-cell `tiel` output named `ZN` rather than `Z`.
* Mux2 `S` select-input vs adder `S` sum-output name collision.
* Top-level `inout` ports being routed as primary inputs by both
  AIG construction and VCD input matching.
* AIG postprocess + sequential decomposition at 200k-cell scale.
* Partitioner / boomerang stage builder at 200k-cell scale.

## Re-using as a regression test

This is currently a manual smoke-test recipe, not a `cargo test`
target — the netlist isn't committed. To wire it up automatically
in CI you would:

1. Cache the chip_top.nl.v artifact (~200 MB).
2. Add an `#[ignore]` test that depends on the artifact path being
   set via an env var.
3. Diff a known-good output VCD signature for a few internal
   signals (requires teaching `gen_stim.v` to instantiate `chip_top`
   and dump some hierarchical wires).
