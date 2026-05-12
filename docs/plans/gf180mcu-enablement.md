# Plan — GF180MCU PDK enablement (full sim path)

**Status:** Proposed. No code landed yet.

**Predecessors:**
- SKY130 enablement (reference recipe in `docs/adding-a-pdk.md`).
- Multi-corner Liberty plumbing — WS2.4 + the sky130 multi-corner
  integration test (`crates/opensta-to-ir/tests/opensta_integration.rs`),
  shipped 2026-05-12.

**ADRs:** None new expected. `docs/adding-a-pdk.md` is the canonical
integration-points checklist; this plan applies that recipe to GF180MCU
with both 7-track (`gf180mcu_fd_sc_mcu7t5v0`) and 9-track
(`gf180mcu_fd_sc_mcu9t5v0`) standard-cell libraries.

## Goal

Bring GF180MCU to the same support tier as SKY130:

1. **Timing path** — `opensta-to-ir` accepts GF180MCU Liberty files
   and emits IR; consumers (jacquard sim with `--timing-ir`) resolve
   per-corner setup/hold/arrival values correctly.
2. **Simulation path** — `jacquard sim` runs a gate-level GF180MCU
   netlist on the GPU. Requires cell-type detection, pin direction
   tables, sequential/tie/multi-output classification, behavioral
   model parsing, and AIG decomposition rules.
3. **Validation** — at minimum a tiny synthetic GF180 DFF+inverter
   fixture mirroring `tests/timing_test/sky130_timing/`; ideally a
   real wafer.space test-run-1 design once one is in hand.

End state mirrors today's SKY130 support: `CellLibrary::GF180MCU`
detected, decomposed to AIG, simulated on Metal/CUDA/HIP, with a
golden-IR corpus entry covering the timing-IR side.

## Why now

GF180MCU support is a release prerequisite per session 2026-05-12. The
wafer.space ecosystem (https://github.com/wafer-space/gf180mcu) is the
near-term commercial demand driver; the upstream
[google/gf180mcu-pdk](https://github.com/google/gf180mcu-pdk) is the
canonical PDK that the wafer.space variant builds on.

## Surface analysis — what SKY130 looks like

| File | Lines | Purpose |
|---|---|---|
| `src/sky130.rs` | 793 | `CellLibrary` enum, library detection, cell-type extraction, `SKY130LeafPins` pin provider |
| `src/sky130_pdk.rs` | 1,654 | Cell classification (sequential, tie, multi-output), behavioral-model parser, AIG decomposition rules |
| `src/aig.rs` | (touched) | `get_sky130_dependencies()`, `sky130_preprocess()`, `sky130_postprocess()` hooks |
| `src/bin/jacquard.rs` | (touched) | CLI match arms |
| `vendor/sky130_fd_sc_hd/` | submodule | Behavioral Verilog cell models |

Total: ~2,447 LOC of new Rust + a vendored submodule. SKY130 detection
covers seven name-prefix variants (hd/hs/ms/ls/lp/hdll/hvl) under a
single `CellLibrary::SKY130` enum value, but only `sky130_fd_sc_hd` is
vendored and fully decomposed.

## Decisions made (2026-05-12 session)

1. **One enum variant for GF180MCU.** `CellLibrary::GF180MCU` covers
   both 7t5v0 and 9t5v0 prefixes. Matches the SKY130 precedent
   (`CellLibrary::SKY130` covers seven prefixes).
2. **Both 7t and 9t fully supported.** Unlike SKY130 (only hd is
   decomposed), both GF180MCU standard-cell variants are first-class
   for cell detection, pin direction, classification, and AIG
   decomposition. The user expects designs to span both.
3. **Two separate submodules** for vendoring cell models, mirroring
   the per-library SKY130 split:
   - `vendor/gf180mcu_fd_sc_mcu7t5v0/`
   - `vendor/gf180mcu_fd_sc_mcu9t5v0/`
   Submodule URLs TBD at Phase 0; either Google ships per-library repos
   (as it does for sky130: `skywater-pdk-libs-sky130_fd_sc_hd`) or we
   pin sub-trees of `google/gf180mcu-pdk`.
4. **Install path:** `volare` already supports `--pdk gf180mcu`; pin a
   single hash under `[tool.jacquard.pdks.gf180mcu]` in
   `pyproject.toml` alongside the existing sky130 entry.
5. **Phasing:** seven phases, each landing as a self-contained commit
   that can be reviewed in isolation. See § Phase breakdown.
6. **wafer.space test-run-1 design** — Phase 7, sequenced after the
   minimum-viable synthetic fixture in Phase 6. Designs and source
   TBD pending availability.

## Phase breakdown

Each phase is one commit unless explicitly split. LOC estimates are
ceilings — actual cell counts may shrink them. Predecessor arrows
indicate hard ordering.

### Phase 0 — Foundations

**Deliverables:**
- `pyproject.toml`: add `[tool.jacquard.pdks.gf180mcu]` table with
  pinned volare hash + variant metadata. Document the install command
  in `docs/timing-validation.md`.
- Two new submodules at `vendor/gf180mcu_fd_sc_mcu7t5v0/` and
  `vendor/gf180mcu_fd_sc_mcu9t5v0/`. Pinned at a known-stable tag.
- Skeleton `src/gf180mcu.rs` + `src/gf180mcu_pdk.rs` with module-level
  doc comments and `#[allow(unused)]` stubs. No real logic.
- Module declarations in `src/lib.rs`.

**Estimated LOC:** ~100 + submodule pins + this plan doc.

**Exit criteria:** `cargo build` clean; submodules initialised;
`uv run volare enable --pdk gf180mcu <hash>` succeeds locally and the
test helper finds the Liberty.

---

### Phase 1 — Library detection + cell-type extraction
**Predecessors:** Phase 0.

**Deliverables:**
- `is_gf180mcu_cell(name) -> bool` in `src/gf180mcu.rs` matching both
  `gf180mcu_fd_sc_mcu7t5v0__` and `gf180mcu_fd_sc_mcu9t5v0__` prefixes
  (and `gf180mcu_fd_io__` / `gf180mcu_fd_pr__` if needed for IO/primitives).
- `extract_cell_type(name)` strips the matching prefix and the drive
  suffix, returning the base cell type. Unit tests covering both
  variants.
- `CellLibrary::GF180MCU` enum value in `src/sky130.rs` (or moved out
  to a neutral location if the enum is renamed — see § Open questions).
- `detect_library()` + `detect_library_from_file()` extended to
  recognise GF180MCU; `Mixed` enforcement updated for three known
  libraries.

**Estimated LOC:** ~200, ~80% tests.

---

### Phase 2 — Pin direction provider
**Predecessors:** Phase 1, Phase 0's vendored submodules.

**Deliverables:**
- `GF180MCULeafPins` struct in `src/gf180mcu.rs` implementing
  `LeafPinProvider`. Returns `Direction` (Input/Output) and pin
  widths for every cell type across both 7t5v0 and 9t5v0.
- Generation strategy: scripted extraction from the behavioural
  Verilog models in the submodules, cross-checked against Liberty
  pin metadata. Generate at build time via `build.rs`. Sky130 takes
  the "commit the table" path, and should be updated to same
  mechanism after this work.
- Round-trip test: parse a synthetic netlist instantiating every
  cell, confirm no unknown-pin errors.

**Estimated LOC:** ~400 (mostly mechanical pin tables).

---

### Phase 3 — Cell classification
**Predecessors:** Phase 2.

**Deliverables:**
- Sequential-cell whitelist in `src/gf180mcu_pdk.rs`:
  `is_sequential_cell()`. Derived from behavioral models — do not
  prefix-match (per the SKY130 lesson re `dlygate4sd3`).
- Tie cells, multi-output cells, hold-time repair buffers identified.
- Reset polarity convention recorded. GF180MCU appears to follow the
  active-high reset convention rather than SKY130's active-low — to
  be confirmed during implementation.
- Unit tests asserting classification across the union of 7t5v0 and
  9t5v0 cell catalogues.

**Estimated LOC:** ~300.

---

### Phase 4 — AIG decomposition
**Predecessors:** Phase 3. **Biggest commit; may split.**

**Deliverables:**
- Behavioral-model parser pulling functional definitions from the
  vendored Verilog (or Liberty) cell models — mirrors
  `parse_functional_model` in `src/sky130_pdk.rs`.
- AIG decomposition rules for every standard-cell type
  (`decompose_with_pdk` equivalent): NAND/NOR/AOI/OAI/inverters/buffers,
  DFFs with various enable/reset combinations.
- Test `test_all_cells_vs_pdk` covering both variants. Sample-based
  exhaustive boolean equivalence — same as SKY130's regression.

**Estimated LOC:** ~1,200. Likely split into Phase 4a (parser + simple
gates) and Phase 4b (complex AOI/OAI + sequential decomposition) if
review pressure demands.

---

### Phase 5 — AIG hooks + CLI wiring
**Predecessors:** Phase 4.

**Deliverables:**
- `src/aig.rs`: `get_gf180mcu_dependencies()`,
  `gf180mcu_preprocess()`, `gf180mcu_postprocess()` (where needed —
  some PDKs need none).
- `src/bin/jacquard.rs`: CLI match arms for `CellLibrary::GF180MCU`
  in the sim / map / cosim paths. Default TimingLibrary loader for
  the typical corner.

**Estimated LOC:** ~150.

---

### Phase 6 — Validation
**Predecessors:** Phase 5.

**Deliverables:**
- Tiny GF180MCU DFF+inverter chain fixture in
  `tests/timing_test/gf180mcu_timing/` mirroring
  `sky130_timing/`. Liberty-only SDF generation script.
- `gf180mcu_multi_corner_emits_per_corner_values` integration test in
  `crates/opensta-to-ir/tests/opensta_integration.rs`. Uses 7t5v0
  Liberty (or 9t5v0 — pick one for the initial pass) across typ/slow/
  fast corners. Same shape as the sky130 test.
- Optional: corpus entry under `tests/timing_ir/corpus/` once the
  install strategy for non-vendored Liberty is finalised. This is
  the same blocker that gates `inv_chain_pnr`.

**Estimated LOC:** ~300 plus a small fixture.

---

### Phase 7 — wafer.space test-run-1 design (deferred)
**Predecessors:** Phase 6; gated on design availability.

**Deliverables:**
- Vendor or pull a wafer.space test-run-1 gate-level netlist into the
  `tests/timing_test/` or `designs/` tree (location TBD based on
  size and license).
- End-to-end pipeline: synth + PnR (or just consume the post-PnR
  output if wafer.space ships it), opensta-to-ir, jacquard sim with
  Metal backend, golden-output VCD comparison.
- Promote to a corpus entry once stable.

Scope and LOC are TBD until the design is in hand.

## Open questions

1. **Submodule URLs.** Does Google ship `gf180mcu_fd_sc_mcu7t5v0` and
   `gf180mcu_fd_sc_mcu9t5v0` as separate `google/gf180mcu-pdk-libs-*`
   repos (the sky130 model), or are they only available as
   subdirectories of `google/gf180mcu-pdk`? If the latter, vendoring
   each as a `git subtree` of the umbrella may be cleaner than
   submodules. **Resolve at Phase 0.**

2. **`CellLibrary` enum location.** It currently lives in
   `src/sky130.rs` even though it represents all PDKs. Moving it to a
   neutral home (`src/pdk.rs` or `src/lib.rs`) is in scope of a small
   refactor; keep that out of the GF180 plan unless a Phase 1 conflict
   forces the issue. **Defer unless forced.**

3. **Reset polarity.** GF180MCU's DFF cells likely use active-high
   reset (the opposite of SKY130's `RESET_B`). Confirm at Phase 3;
   add a `dff_reset_active_low()` trait or per-PDK constant if both
   conventions need first-class support.

4. **IO and PR libraries.** `gf180mcu_fd_io` (pads, levelshifters)
   and `gf180mcu_fd_pr` (primitives — diodes, R/C, antenna cells) are
   present in real post-P&R netlists. Treat them like
   `tap`/`fill`/`decap` cells (recognised but stubbed) until the
   wafer.space test-run-1 design forces a richer model.

5. **9t5v0 cell coverage.** The 9t library may have cells that 7t
   doesn't (different drive strengths, alternate footprints).
   Validate cell-name lists are union-compatible at Phase 2.

6. **CI install strategy.** Same blocker as the sky130 corpus entry
   (handoff §5). The local-dev test path works today via volare; CI
   integration follows when the GPU-runner work resumes.

## Pitfalls (PDK-specific)

Carried forward from `docs/adding-a-pdk.md` § Common Pitfalls, with
GF180MCU-specific calls:

- **Reset polarity** — see Open Q3.
- **Power pins** — GF180MCU operates at 5V nominal (vs SKY130's
  1.8V). Both follow VDD/VSS naming, but Liberty parameter ranges
  differ. Make sure the corner naming (`tt_025C_5v00` etc.) maps
  cleanly through the existing TimingLibrary loader.
- **Cell name collisions** between 7t5v0 and 9t5v0 — both have
  `nand2_1` etc. Detection must key on the full prefix, not the base
  type.
- **Drive-strength suffix differences.** SKY130 uses integer
  multipliers (`inv_1`, `inv_2`, `inv_4`). GF180MCU appears to follow
  the same convention but verify against the actual cell names in
  the submodule.

## Migration / cleanup at completion

- Plan doc resolves: fold the closed phases' content into the
  `docs/adding-a-pdk.md` reference (or into a `docs/pdk-support.md`
  index) as a worked second example. Delete this plan doc.
- `CellLibrary::Mixed` semantics extended to three libraries — verify
  no callers assume binary AIGPDK/SKY130 only.
- Submodule pins recorded in `docs/release-process.md` § License
  posture (both gf180mcu libraries are Apache-2.0 upstream).

## Links

- `docs/adding-a-pdk.md` — canonical recipe (SKY130 reference).
- `src/sky130.rs`, `src/sky130_pdk.rs` — the reference implementation.
- `crates/opensta-to-ir/tests/opensta_integration.rs::sky130_multi_corner_emits_per_corner_values`
  — the timing-side validation pattern to mirror in Phase 6.
- `pyproject.toml::[tool.jacquard.pdks.sky130]` — install-pin
  pattern to mirror.
- Upstream PDK: https://github.com/google/gf180mcu-pdk
- wafer.space variant: https://github.com/wafer-space/gf180mcu
