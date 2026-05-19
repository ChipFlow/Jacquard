# Plan — GF180MCU PDK enablement (full sim path)

**Status:** Phases 0–6 shipped (2026-05-12 / 13). Phase 7
(wafer.space test-run-1 design integration) deferred pending design
availability. Subsequent follow-ups also landed (2026-05-14):
IO pad behavioural decomposition (`__in_c`, `__in_s`, `__bi_24t`,
plus filler classification for the wafer.space `gf180mcu_ws_*`
families) and bidir A/OE observability surfacing as
`<port>__out` / `<port>__oe` extra primary outputs — see commits
`aa312b8`, `c23d583`, `207cc80`. These extended GF180MCU support
from "synthesized-core-only" to "full chip_top including pad ring",
validated end-to-end on a 227k-cell wafer.space chess chip_top
netlist. This document is now a recap of what landed; the
forward-looking deferred items are in § Follow-on cleanup at the
bottom.

**Predecessors:**
- SKY130 enablement (reference recipe in `docs/adding-a-pdk.md`).
- Multi-corner Liberty plumbing — WS2.4 + the sky130 multi-corner
  integration test (`crates/opensta-to-ir/tests/opensta_integration.rs`),
  shipped 2026-05-12.

**ADRs:** None new shipped. `docs/adding-a-pdk.md` is the canonical
integration-points checklist; this plan applied that recipe to
GF180MCU with both 7-track (`gf180mcu_fd_sc_mcu7t5v0`) and 9-track
(`gf180mcu_fd_sc_mcu9t5v0`) standard-cell libraries.

## Goal (as shipped)

GF180MCU is now at the same support tier as SKY130:

1. **Timing path** — `opensta-to-ir` accepts GF180MCU Liberty files
   and emits IR; the multi-corner integration test at
   `crates/opensta-to-ir/tests/opensta_integration.rs::gf180mcu_multi_corner_emits_per_corner_values`
   asserts per-corner setup/hold values differ correctly across
   tt/ss/ff PVT corners.
2. **Simulation path** — `jacquard sim` runs gate-level GF180MCU
   netlists on the GPU. Cell-type detection, pin direction tables,
   sequential/tie/multi-output classification, behavioural model
   parsing (with UDP support for sequential elements), and AIG
   decomposition are all wired through `AIG::from_netlistdb`.
3. **Validation** — synthetic DFF+inverter fixture at
   `tests/timing_test/gf180mcu_timing/`. Real wafer.space test-run-1
   design integration is deferred (Phase 7, gated on design
   availability).

End state mirrors today's SKY130 support: `CellLibrary::GF180MCU`
detected, decomposed to AIG, simulated on Metal/CUDA/HIP, with a
golden-IR corpus entry covering the timing-IR side.

## Why now

GF180MCU support was a release prerequisite per session 2026-05-12.
The wafer.space ecosystem (https://github.com/wafer-space/gf180mcu)
is the near-term commercial demand driver; the upstream
[google/gf180mcu-pdk](https://github.com/google/gf180mcu-pdk) is the
canonical PDK that the wafer.space variant builds on.

## Decisions (frozen 2026-05-12 session)

1. **One enum variant for GF180MCU.** `CellLibrary::GF180MCU` covers
   both 7t5v0 and 9t5v0 prefixes. Matches the SKY130 precedent
   (`CellLibrary::SKY130` covers seven prefixes).
2. **Both 7t and 9t fully supported.** Unlike SKY130 (only hd is
   decomposed), both GF180MCU standard-cell variants are first-class
   for cell detection, pin direction, classification, and AIG
   decomposition. Cell models for 7t and 9t are byte-identical per
   cell type (verified at build time in `build.rs`); decomposition
   reads from the 7t submodule and reuses for 9t.
3. **Two separate submodules** for vendoring cell models, mirroring
   the per-library SKY130 split:
   - `vendor/gf180mcu_fd_sc_mcu7t5v0/`
   - `vendor/gf180mcu_fd_sc_mcu9t5v0/`
4. **Install path:** `volare` pinned hash under
   `[tool.jacquard.pdks.gf180mcu]` in `pyproject.toml` alongside the
   existing sky130 entry. Variant: `gf180mcuC`.
5. **Reset polarity:** GF180MCU uses **active-low** resets/sets
   (pin names `RN`, `SETN`) — same AIG formula shape as SKY130's
   `RESET_B`/`SET_B`. The "n" *prefix* in cell names like
   `dffnq`/`dffnrnq`/`icgtn` indicates a **negative-edge clock**
   (pin `CLKN`), not reset polarity (resolving Open Q3 from the
   original plan).

## Shipped phases

### Phase 0 — Foundations (commit `6ae3e54`)

- `pyproject.toml`: `[tool.jacquard.pdks.gf180mcu]` with
  `volare_hash = "559a117b163cef2f920f33f30f6f690aa0b47e4c"`, variant
  `gf180mcuC`, separate `default_lib_subdir_7t` /
  `default_lib_subdir_9t` paths.
- Vendored submodules at `vendor/gf180mcu_fd_sc_mcu7t5v0/` and
  `vendor/gf180mcu_fd_sc_mcu9t5v0/`.
- Skeleton `src/gf180mcu.rs` + `src/gf180mcu_pdk.rs` declared in
  `src/lib.rs`.

### Phase 1 — Library detection + cell-type extraction (commit `858dd70`)

- `is_gf180mcu_cell(name) -> bool` matching both 7t5v0 and 9t5v0
  prefixes.
- `extract_cell_type(name)` strips prefix + drive suffix.
- `CellLibrary::GF180MCU` enum value added; `detect_library()` /
  `detect_library_from_file()` extended; `Mixed` enforcement
  upgraded to three known libraries.

### Phase 2 — Pin direction provider (commit `e97e2d2`)

- `GF180MCULeafPins` implementing `LeafPinProvider`.
- Generation strategy: **build-time** via `build.rs::generate_gf180mcu_pin_table`,
  which scans `vendor/gf180mcu_fd_sc_mcu{7,9}t5v0/cells/`, parses
  `.functional.v`, cross-asserts 7t/9t pin layouts match, emits
  `$OUT_DIR/gf180mcu_pins.rs`. New precedent vs SKY130's
  hand-rolled match arms (see § Follow-on cleanup item 1).
- Round-trip test instantiating every cell.

### Phase 3 — Cell classification (commit `6969b90`)

- Sequential / tie / filler / delay-cell whitelists in
  `src/gf180mcu_pdk.rs` derived from behavioural models.
- Unit tests asserting classification across the union of 7t5v0 and
  9t5v0 cell catalogues.

### Phase 4 — Combinational AIG decomposition

Sequenced as four commits:

- `92bb665` — Phase 4 recon: confirmed SKY130 behavioural parser is
  PDK-neutral; identified shared infrastructure that
  `gf180mcu_pdk` could reuse.
- `02da077` — Phase 4 prep: introduced the PDK-neutral
  `src/pdk_decomp.rs` re-export module; exposed `WireVal`,
  `GATE_MARKER`, `build_chain_gate`, `build_xor_chain`,
  `finalize_decomp_result` as `pub(crate)`.
- `32fb3b9` — Phase 4 (combinational): `decompose_combinational` for
  GF180MCU + boolean equivalence test suite vs the vendored PDK
  models.
- `d898343` — Phase 4 (aig.rs integration): wired combinational
  decomposition through `AIG::from_netlistdb`, end-to-end sim path
  for combinational GF180MCU netlists.

### Phase 4b — Sequential cells (UDPs)

- `a7c0618` — Phase 4b prep: UDP loader for `gf180mcu_pdk`
  (parses `UDP_GF018hv5v_mcu_sc7_TT_1P8V_25C_verilog_nonpg_*_FF_UDP`
  and friends from the vendored PDK).
- `459317e` — Phase 4b: AIG hooks for sequential cells (DFFs,
  latches, scan-DFFs, clock-gating cells `icgtp`/`icgtn`).
  `gf180mcu_preprocess` pre-creates DFF Q pins; `gf180mcu_postprocess`
  applies async set/reset using the active-low RN/SETN convention
  via the same AIG formula as SKY130. Negative-edge clock cells
  use `CLKN` instead of `CLK` (handled in `trace_clock_pin`).
- `3006f59` — Phase 4b boolean-equivalence tests covering DFF,
  latch, scan-DFF, and clock-gating cells via multi-step
  truth-table evaluation.

### Phase 5 — CLI / pipeline wiring audit (commit `57244d5`)

Audit-only — no per-PDK branch was missing GF180MCU handling. The
auto-detection in `AIG::from_netlistdb` already covers every CLI
surface (`sim` / `cosim` / `dump-paths` all route through
`setup::load_design`). Cleanup: stale Phase 4b panic comments in
`src/sim/setup.rs` and `src/aig.rs`; field doc comments on CLI
arguments refreshed to mention GF180MCU alongside AIGPDK / SKY130.

### Phase 6 — Validation fixture + multi-corner test

- **Fixture** (commit `4a7ee0e`): `tests/timing_test/gf180mcu_timing/`
  mirroring `sky130_timing/` 1:1. Synthetic `inv_chain.v` (DFF +
  16-inverter chain + DFF) with `gf180mcu_fd_sc_mcu7t5v0__{dffq,inv}_1`
  cells, Liberty-only SDF generator, CVC testbench, sample stimulus,
  Makefile, README.
- **Integration test**: `gf180mcu_multi_corner_emits_per_corner_values`
  in `crates/opensta-to-ir/tests/opensta_integration.rs`. Loads
  three real PVT corners (typ=`tt_025C_5v00`, slow=`ss_125C_4v50`,
  fast=`ff_n40C_5v50`) at the 5.0 V operating point and asserts
  per-corner setup TimingValues differ correctly across PVT.
  Skips gracefully when the volare-installed PDK isn't present
  (gated on `find_gf180mcu_lib_dir()` returning `Some`; matches
  the sky130 test's skip pattern). `$GF180MCU_LIBERTY_DIR` overrides
  the volare default path.

### Phase 7 — wafer.space test-run-1 design (deferred)

Gated on design availability. Scope:

- Vendor or pull a wafer.space test-run-1 gate-level netlist into
  the `tests/timing_test/` or `designs/` tree.
- End-to-end pipeline: synth + PnR (or consume post-PnR output),
  opensta-to-ir, jacquard sim with Metal backend, golden-output
  VCD comparison.
- Promote to a corpus entry once stable.

## Test inventory

Counts after Phase 6:

- `cargo test --lib`: **212 passing** (up from 166 at plan start).
- `cargo test --lib gf180mcu`: **45 passing** (combinational + sequential
  equivalence + classification + detection + AIG-build).
- `cargo test -p opensta-to-ir multi_corner`: **2 passing** (sky130 +
  gf180mcu), each gated on its respective volare PDK install.

## Follow-on cleanup

These are nice-to-have refactors flagged during the GF180MCU work
but deliberately out of scope for the enablement effort itself.

**Update 2026-05-19:** Items 1, 2, and 4 are now subsumed by
[ADR 0010 — Declarative cell metadata](../adr/0010-declarative-cell-metadata.md)
and its companion plan `declarative-cell-metadata.md`. The manifest
pathway converts these from "Rust refactor" projects into "move data
out of code as part of the migration to manifest-as-source-of-truth"
— happens once, gets all three at once.

1. **~~`build.rs` pin-table generator for SKY130 too.~~** Subsumed by
   ADR 0010 § "Deferred to a future ADR — `build.rs` pin-table
   scanner removal." Removed LAST in the manifest migration, after
   manifests cover the built-in PDKs.

2. **~~Physical relocation of shared PDK decomp infrastructure~~** out
   of `sky130_pdk.rs` into `pdk_decomp.rs`. Still relevant for the
   built-in (Rust-decomp) pathway, since ADR 0010 keeps that path
   load-bearing for cells with real AIG decomposition rules. Move
   when a third PDK exercises the surface.

3. **`CellLibrary` enum location.** Currently lives in `src/sky130.rs`
   even though it represents all PDKs. Moving to a neutral home
   (`src/pdk.rs` or `src/lib.rs`) is a trivial mechanical refactor.
   Independent of ADR 0010.

4. **~~IO and PR libraries.~~** Now solved by the ADR 0010 manifest
   pathway. `gf180mcu_fd_io` and `gf180mcu_fd_pr` cells can be
   declared via `kind = "io_pad_*"` / `kind = "filler"` / `kind =
   "tap"` etc. in user-supplied manifests — no Jacquard PR needed.

5. **CI install strategy for GF180MCU Liberty.** Both the sky130 and
   gf180mcu multi-corner tests currently skip when the PDK isn't
   installed locally. CI integration (volare-on-CI or a vendored
   minimal Liberty subset) is the same blocker that gates the
   `inv_chain_pnr` sky130 corpus entry — out of scope for the GF180
   enablement effort itself. Unrelated to ADR 0010.

## Pitfalls (PDK-specific, for future readers)

- **Reset polarity** — GF180MCU is active-low (`RN`/`SETN`); same
  AIG formula as SKY130's `RESET_B`/`SET_B`.
- **Negative-edge clocks** — cells like `dffnq`/`dffnrnq`/`icgtn`
  use pin name `CLKN` instead of `CLK`. The "n" prefix is a clock
  marker, not a reset-polarity marker.
- **Power pins** — GF180MCU operates at 5V nominal (vs SKY130's
  1.8V). Both follow VDD/VSS naming. Corner names follow
  `tt_025C_5v00` shape and parse cleanly through the generic
  `TimingLibrary` loader.
- **Cell pin names differ from SKY130** — inverter is `I`/`ZN`
  (not `A`/`Y`); DFF is `CLK`/`D`/`Q`/`notifier`. The `notifier`
  port wires the UDP delay-model wrapper but is unused for logic
  simulation.
- **Cell-name collisions** between 7t5v0 and 9t5v0 — both have
  `nand2_1` etc. Detection keys on the full prefix, not the base
  type. Auto-handled by `is_gf180mcu_cell`.
- **Drive-strength suffixes** — GF180MCU uses integer multipliers
  (`inv_1`, `inv_2`, `inv_4`, …) matching the SKY130 convention.

## Links

- `docs/adding-a-pdk.md` — canonical PDK integration recipe.
- `src/sky130.rs`, `src/sky130_pdk.rs` — SKY130 reference
  implementation.
- `src/gf180mcu.rs`, `src/gf180mcu_pdk.rs` — GF180MCU
  implementation.
- `crates/opensta-to-ir/tests/opensta_integration.rs::{sky130,gf180mcu}_multi_corner_emits_per_corner_values`
  — timing-side validation.
- `tests/timing_test/{sky130,gf180mcu}_timing/` — synthetic
  fixtures.
- `pyproject.toml::[tool.jacquard.pdks.{sky130,gf180mcu}]` —
  install pins.
- Upstream PDK: https://github.com/google/gf180mcu-pdk
- wafer.space variant: https://github.com/wafer-space/gf180mcu
