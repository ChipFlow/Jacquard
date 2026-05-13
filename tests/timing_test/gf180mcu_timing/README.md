# GF180MCU Timing Test Suite

Synthetic test circuits using GF180MCU 7t5v0 standard cells with
analytically verifiable timing properties. Mirrors `sky130_timing/`
1:1 — same shape, same scripts, same workflow, but exercises the
GlobalFoundries 180 nm PDK rather than SkyWater 130 nm.

## Test Circuits

### 1. `inv_chain.v` — Inverter Chain
- **Circuit**: `dffq_1` -> 16 x `inv_1` -> `dffq_1`
- **Logic function**: Identity (even number of inversions)
- **Expected combo delay**: 16 x ~45 ps ≈ 720 ps (Liberty min slew, typ corner)
- **Expected arrival at capture DFF**: clk_to_q + 16 x inv_delay
- **Purpose**: Validates basic delay accumulation through a linear chain
  with one sequential boundary at each end.

The fixture deliberately keeps to the smallest possible shape — one
sequential boundary, one combinational path — so the timing semantics
are easy to audit by hand.

## Cells Used

| Cell | Pins | Function |
|---|---|---|
| `gf180mcu_fd_sc_mcu7t5v0__dffq_1` | CLK, D, Q, notifier | Positive-edge DFF with Q output |
| `gf180mcu_fd_sc_mcu7t5v0__inv_1`  | I, ZN              | Inverter (note: pin names differ from SKY130's A/Y) |

Cell layouts are byte-identical between `gf180mcu_fd_sc_mcu7t5v0`
and `gf180mcu_fd_sc_mcu9t5v0`; only the 7t variant is exercised here
to keep cross-library validation under `crates/opensta-to-ir`.

## Reference Timing Values

Approximate Liberty values at the typical corner
(`tt_025C_5v00`, minimum input slew / minimum output load):

| Parameter | Value (ps) |
|-----------|-----------|
| `inv_1` cell_rise / cell_fall | ~38 (min load) / ~50 (typ load) |
| `dffq_1` clk_to_q (rising_edge IOPATH) | ~320 |
| `dffq_1` setup_rising | ~230 |
| `dffq_1` hold_rising | ~86 |

Exact values depend on the Liberty operating point. The SDF generation
script reads representative values at the smallest table index, which
is conservative (faster delays than real-world loaded cells).

## Pre-Layout (Liberty-Only) Timing Validation

### Generate Liberty-Only SDF Files

```sh
python3 gen_liberty_sdf.py inv_chain.v
```

This creates `inv_chain.sdf` with delay specs extracted from the
GF180MCU Liberty file. Pre-layout SDF has no routing parasitics —
only combinational path delays + setup/hold from the library timing
tables.

### Run with CVC

```sh
cvc64 +typdelays tb_inv_chain.v inv_chain.v
./cvcsim
```

The same workflow as `sky130_timing/`; CVC understands GF180MCU cell
models the same way it does SKY130's.

### Compare Jacquard vs CVC

Use the parent-directory comparison script:

```sh
bash ../compare_timing.py cvc_output.vcd jacquard_output.vcd
```

## Multi-Corner Liberty Validation

The corresponding multi-corner integration test lives at:

```
crates/opensta-to-ir/tests/opensta_integration.rs::gf180mcu_multi_corner_emits_per_corner_values
```

It uses three real GF180MCU corners (`tt_025C_5v00`, `ss_125C_4v50`,
`ff_n40C_5v50`) and asserts the per-corner setup/hold/arrival values
differ across PVT. The test skips cleanly when the volare PDK isn't
installed (set `$GF180MCU_LIBERTY_DIR` to override the lookup path).

## Files

- `inv_chain.v` — Inverter chain (pre-layout RTL with GF180MCU cells)
- `tb_inv_chain.v` — CVC testbench for inv_chain timing measurements
- `gen_liberty_sdf.py` — Script to generate Liberty-only SDF from RTL
- `Makefile` — Build orchestration
- `*.sdf` — Generated Liberty-only SDF files (post-generation)
