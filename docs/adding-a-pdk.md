# Adding a New PDK for Post-Layout Simulation

This guide documents the process of enabling a new process design kit (PDK) for
gate-level simulation in Jacquard. It is based on the SKY130 enablement and captures
every integration point.

## Overview

Jacquard natively supports AIGPDK (its own synthesis library of AND gates, DFFs, and
SRAMs). Supporting a foundry PDK like SKY130 requires teaching the simulator how
to interpret the PDK's standard cells: their pin directions, their boolean
function, and which ones are sequential.

There are now **two pathways** for enabling new cells; pick based on what
you're adding:

- **Built-in PDK enablement** (this guide). For full standard-cell
  libraries — AND gates, DFFs, sequential cells with explicit AIG
  decomposition rules. Requires Rust code: pin tables, classifiers,
  decomposition functions, AIG builder hooks.
- **Runtime cell library** (`--cell-library` + `.cells.toml`
  manifest). For third-party IP, hard macros, foundry memories, and
  any other cells that *don't* need new AIG decomposition rules — i.e.
  cells that act as opaque outputs (RAM macros), filler/cap blocks,
  or IO pads. See **ADR 0010** and `docs/plans/declarative-cell-metadata.md`
  for the recipe. **No Jacquard PR required** — users ship a manifest
  alongside their netlist.

This guide covers the built-in pathway, which touches five areas:

1. **Library detection** -- recognizing cell names from a netlist
2. **Pin direction provider** -- telling the netlist parser which pins are inputs/outputs
3. **Cell classification** -- identifying sequential, tie, and multi-output cells
4. **Behavioral decomposition** -- converting PDK cells to AIG (AND/NOT) primitives
5. **CLI wiring** -- connecting it all together

If you're adding just a memory macro or other behaviourally-opaque IP,
**skip ahead to "Adding third-party IP via runtime manifest"** at the
end of this document — it's a 6-line TOML entry, not a Rust PR.

## Prerequisites

You need:

- The PDK's Verilog cell library (behavioral or functional models)
- A post-synthesis or post-P&R netlist using those cells
- The cell naming convention (prefix, drive strength suffix format)

For SKY130, the PDK data lives in `vendor/sky130_fd_sc_hd/` as a git submodule.

## Step 1: Library Detection

**Reference**: `src/sky130.rs` -- `is_sky130_cell()`, `detect_library()`,
`detect_library_from_file()`

Jacquard scans the netlist to determine which cell library is in use. Each PDK needs
a name-matching function:

```rust
// src/sky130.rs:535
pub fn is_sky130_cell(name: &str) -> bool {
    name.starts_with("sky130_fd_sc_")
        || name.starts_with("CF_SRAM_")
}
```

The `CellLibrary` enum tracks known libraries. `detect_library()` iterates cell
names and returns the detected library (or `Mixed` if cells from multiple
libraries are found -- this is an error).

**For a new PDK**: Add a variant to `CellLibrary`, write an `is_<pdk>_cell()`
function, and update `detect_library()`.

## Step 2: Cell Type Extraction

**Reference**: `src/sky130.rs` -- `extract_cell_type()`

PDK cell names follow a convention: `<prefix>__<type>_<drive>`. The simulator
needs to strip the prefix and drive strength to get the base cell type:

```
sky130_fd_sc_hd__nand2_4  -->  nand2
sky130_fd_sc_hd__dfxtp_1  -->  dfxtp
```

This function must handle all library variants (hd, hs, ms, ls, lp, hdll, hvl
for SKY130) and any custom macros (CF_SRAM_*).

**For a new PDK**: Write an equivalent `extract_cell_type()` for the PDK's
naming scheme.

## Step 3: Pin Direction Provider

**Reference**: `src/sky130.rs` -- `SKY130LeafPins` implementing `LeafPinProvider`

The netlist parser (from `eda-infra-rs/netlistdb`) needs to know pin
directions and widths for every cell type. This is implemented as a trait:

```rust
impl LeafPinProvider for SKY130LeafPins {
    fn direction_of(&self, macro_name, pin_name, pin_idx) -> Direction;
    fn width_of(&self, macro_name, pin_name) -> Option<SVerilogRange>;
}
```

For SKY130, `direction_of()` is a large match statement covering ~80 cell types
with all their pin names. This is tedious but straightforward -- for each cell,
list which pins are inputs and which are outputs.

**Sources for pin directions**:
- The PDK's Liberty (.lib) files list pin directions
- The PDK's behavioral Verilog models declare `input`/`output` ports
- LEF files also contain pin direction information

**For a new PDK**: Implement the trait for all cells that appear in your target
netlists. You can start with just the cells used in your design and add others
as needed.

## Step 4: Cell Classification

**Reference**: `src/sky130_pdk.rs` -- `is_sequential_cell()`, `is_tie_cell()`,
`is_multi_output_cell()`

Three classification functions control how cells are processed during AIG
construction:

### Sequential cells (DFFs and latches)

These are handled specially in the AIG builder -- their outputs become state
elements rather than combinational logic.

**Critical**: Use an explicit whitelist, not prefix matching. PDK naming
collisions will silently break simulation if you guess wrong (e.g., SKY130's
`dlygate4sd3` starts with "dl" but is a combinational delay buffer, not a
latch).

**Derivation method**: Grep the PDK's behavioral Verilog models for DFF/latch
primitives:

```bash
for cell in $(ls vendor/<pdk>/cells/); do
    vfile="vendor/<pdk>/cells/$cell/<pdk>__${cell}.behavioral.v"
    if [ -f "$vfile" ] && grep -qE 'udp_dff|udp_dlatch' "$vfile"; then
        echo "$cell"
    fi
done
```

For PDKs that don't use Verilog UDPs, look for `always @(posedge` blocks or
check the Liberty file's `ff` and `latch` groups.

### Tie cells

Cells that produce constant 0 or 1 (e.g., SKY130's `conb` with HI/LO pins).

### Multi-output cells

Cells with more than one output (e.g., half-adder `ha` with SUM and COUT,
full-adder `fa`). These need special handling because the AIG builder processes
one output pin at a time.

## Step 5: Behavioral Model Loading

**Reference**: `src/sky130_pdk.rs` -- `load_pdk_models()`, `parse_functional_model()`,
`parse_udp()`

Jacquard decomposes PDK cells to AIG primitives (AND gates and inversions) by
parsing their functional Verilog models. The expected file structure:

```
vendor/<pdk>/
  cells/
    <cell_type>/
      <pdk>__<cell_type>.functional.v    # Gate-level behavioral model
  models/
    <udp_name>/
      <pdk>__<udp_name>.v               # Verilog UDP definitions
```

### Functional models

These are gate-level Verilog using primitives like `and`, `or`, `nand`, `nor`,
`not`, `xor`, `xnor`, `buf`. The parser (`parse_functional_model()`) extracts
these into a topologically-ordered list of `BehavioralGate` structures.

Example (`sky130_fd_sc_hd__o21ai.functional.v`):
```verilog
module sky130_fd_sc_hd__o21ai (Y, A1, A2, B1);
    output Y;
    input  A1, A2, B1;
    wire or0_out;
    or  or0  (or0_out, A2, A1);
    nand nand0 (Y, B1, or0_out);
endmodule
```

### UDP models

Some cells (typically muxes) use Verilog User-Defined Primitives with truth
tables. The parser (`parse_udp()`) converts these to a row-based representation,
which is then evaluated as sum-of-products during AIG decomposition.

### What's loaded

Only models for cell types actually present in the design are loaded. Sequential
cells are skipped (their behavior is hardcoded in the AIG builder). Tie cells
are also skipped (constant generation is trivial).

**For a new PDK**: If the PDK uses the same Verilog gate primitive syntax, the
existing parsers should work. If it uses behavioral Verilog (`assign` statements,
`always` blocks), the parser would need extension.

## Step 6: AIG Decomposition

**Reference**: `src/sky130_pdk.rs` -- `decompose_with_pdk()`,
`decompose_from_behavioral()`

The decomposition converts each combinational cell to a set of 2-input AND gates
with optional inversions:

1. Map the cell's input pin names to AIG pin indices via `CellInputs`
2. Walk the behavioral model's gate list in topological order
3. For each gate, build the equivalent AIG sub-graph:
   - `and`/`nand` -> AND gate (with optional output inversion)
   - `or`/`nor` -> De Morgan's: `OR(a,b) = NOT(AND(NOT a, NOT b))`
   - `xor`/`xnor` -> Four AND gates: `XOR(a,b) = NOT(AND(NOT(AND(a, NOT b)), NOT(AND(NOT a, b))))`
   - `buf`/`not` -> Pass-through with optional inversion
   - UDP -> Sum-of-products from truth table
4. Record the output with cell origin (for SDF timing annotation)

### CellInputs struct

`CellInputs` has named fields for all possible input pins across all SKY130
cells (A, B, C, D, A_N, B_N, S, S0, S1, CIN, SET_B, RESET_B, etc.). The
`set_pin()` method maps netlist pin names to AIG pin indices.

**For a new PDK**: If the PDK introduces pin names not in the current struct,
add new fields.

## Step 7: AIG Builder Integration

**Reference**: `src/aig.rs` -- `get_sky130_dependencies()`, `sky130_preprocess()`,
`sky130_postprocess()`

The AIG builder processes cells in three phases during topological traversal:

### Dependencies (what must be built before this cell)

- **Tie cells**: No dependencies
- **Sequential cells**: Only SET_B and RESET_B pins (the data input D is handled
  by the DFF mechanism, not combinational decomposition)
- **Combinational cells**: All input pins

### Preprocessing (before dependencies are resolved)

- **Sequential cells**: Create a DFF output AIG pin. This establishes the state
  element before the combinational cone driving it is built.

### Postprocessing (after all dependencies are resolved)

- **Tie cells**: Wire `HI` to constant-1, `LO` to constant-0
- **Sequential cells**: Apply reset/set logic:
  `Q = AND(OR(Q_state, NOT SET_B), RESET_B)` (active-low semantics)
- **Combinational cells**: Call `decompose_with_pdk()` and wire the resulting
  AND gates into the AIG

**For a new PDK**: The three-phase structure is reusable. You need PDK-specific
implementations of each phase that handle the new cell types' pin names and
reset/set conventions.

## Step 8: CLI Integration

**Reference**: `src/bin/jacquard.rs`

The `load_design` function detects the library and creates the netlist with the
appropriate pin provider:

```rust
let lib = detect_library_from_file(&args.netlist_verilog)?;
let netlistdb = match lib {
    CellLibrary::SKY130 => NetlistDB::from_sverilog_file(&paths, &SKY130LeafPins),
    CellLibrary::AIGPDK => NetlistDB::from_sverilog_file(&paths, &AIGPDKLeafPins()),
    CellLibrary::Mixed => panic!("Mixed libraries not supported"),
};
```

**For a new PDK**: Add a match arm for the new library.

## Testing Strategy

### Unit tests

1. **Cell type extraction**: Verify prefix/suffix stripping
2. **Pin directions**: Spot-check common cells
3. **Behavioral model parsing**: Parse each cell type, verify gate count
4. **Decomposition correctness**: For each combinational cell, exhaustively
   test all input combinations against the PDK's truth table:

   ```rust
   #[test]
   fn test_all_cells_vs_pdk() {
       let pdk = load_test_pdk();
       for (cell_type, model) in &pdk.models {
           // For each input combination:
           //   1. Evaluate behavioral model directly
           //   2. Decompose to AIG and evaluate AIG
           //   3. Assert outputs match
       }
   }
   ```

   This test exists in `src/sky130_pdk.rs` as `test_all_cells_vs_pdk` and
   covers every combinational cell against every input combination.

### Integration tests

1. **Small test circuit**: Synthesize a simple design (DFF + some gates) to the
   new PDK and verify simulation output matches a reference (e.g., iverilog)
2. **Flash boot test**: If targeting an SoC, verify the CPU boots and reads from
   flash (this exercises sequential logic, combinational cones, and IO)

## File Checklist

For a complete PDK integration, you need:

| File | Purpose |
|------|---------|
| `src/<pdk>.rs` | LeafPinProvider, library detection, cell type extraction |
| `src/<pdk>_pdk.rs` | Cell classification, model parsing, AIG decomposition |
| `src/aig.rs` | AIG builder hooks (dependencies, pre/post-process) |
| `src/sky130.rs` | Update `CellLibrary` enum |
| `src/bin/jacquard.rs` | CLI match arms for new library |
| `vendor/<pdk>/` | PDK cell models (git submodule) |

## Common Pitfalls

- **Cell name collisions**: Do not use prefix matching for cell classification.
  `dlygate4sd3` starts with "dl" but is not a latch. Always derive the
  exhaustive list from behavioral models.

- **Active-low vs active-high resets**: SKY130 uses active-low `RESET_B` and
  `SET_B`. Other PDKs may use active-high. Get this wrong and every DFF will
  be stuck.

- **Multi-output cells**: The AIG builder processes one output pin at a time.
  If a cell has both Q and Q_N outputs (e.g., `dfbbp`), the second output
  must be derived from the first (Q_N = NOT Q), not decomposed independently.

- **Liberty file size**: SKY130's liberty files are 12MB+. If your PDK has
  similarly large files, ensure the parser doesn't OOM or timeout.

- **Power/ground pins**: Post-layout netlists often include VPWR/VGND pins.
  Use the unpowered netlist variant (`.nl.v` not `.pnl.v` in OpenLane2) or
  handle power pins as constants in the pin provider.

- **Hold-time repair buffers**: P&R tools insert delay buffers (like
  `dlygate4sd3`) that must be treated as combinational. If your PDK's delay
  cells have names that collide with sequential cell prefixes, the whitelist
  approach prevents misclassification.

## Adding third-party IP via runtime manifest

If you're adding a memory macro, IO pad, hard block, or filler library
— anything that doesn't need new AIG decomposition rules — the runtime
cell-library pathway (ADR 0010) is the right route. **No Jacquard PR
required.** Ship a Verilog blackbox file plus a TOML manifest alongside
your design.

### Step 1: Provide the cell's Verilog interface

The blackbox just declares the cell's module + port directions. The
foundry typically ships this (`<library>__blackbox.v`). Example for the
OCD GF180MCU SRAM:

```verilog
module gf180mcu_ocd_ip_sram__sram1024x8m8wm1 (CLK, CEN, GWEN, WEN, A, D, Q);
  input CLK;
  input CEN;
  input GWEN;
  input [7:0] WEN;
  input [9:0] A;
  input [7:0] D;
  output [7:0] Q;
endmodule
```

### Step 2: Write the TOML manifest

Co-locate `<library>.cells.toml` next to `<library>.v` (it autoloads
when present) or pass it via `--cell-manifest`:

```toml
schema_version = "1.0"

[cells.gf180mcu_ocd_ip_sram__sram1024x8m8wm1]
kind = "ram"
```

Recognised `kind` values in v1.0: `std`, `dff`, `latch`, `clock_gate`,
`ram`, `filler`, `endcap`, `tap`, `io_pad_input`, `io_pad_output`,
`io_pad_bidir`, `delay`, `multi_output`, `tie_high`, `tie_low`.

### Step 3: Invoke jacquard with the manifest

```sh
jacquard sim my_chip.v stim.vcd out.vcd 1 \
    --cell-library deps/gf180mcu_ocd_ip_sram/cells/gf180mcu_ocd_ip_sram__sram1024x8m8wm1/gf180mcu_ocd_ip_sram__sram1024x8m8wm1__blackbox.v
```

The `--cell-library` flag is repeatable for multi-IP designs.

### What `kind = "ram"` delivers in v1.0

The cell's output pins become X-source slots in the AIG. The SRAM's
internal memory behaviour is **not** modelled in v1.0 — sufficient for
designs whose CPU executes from boot ROM / register file and never
reads SRAM contents at the timescales Jacquard simulates
(the heartbeat-verification case driving this work). Real memory
modelling lands in a future
schema version with explicit port-mapping (`[cells.NAME.ports]`).

### Other kinds

- `filler`, `endcap`, `tap` — physical-only, contribute no logic.
- `io_pad_input` / `io_pad_output` / `io_pad_bidir` — pad-level
  behaviour (parallel to the built-in `gf180mcu_ws_io__*` family).
- `dff`, `latch`, `clock_gate`, `delay`, `multi_output` — recognised
  but the v1.0 schema doesn't yet expose enough port semantics to
  drive AIG construction for these. Coming in the port-mapping
  schema (future ADR). For now, declaring these kinds documents
  intent without changing behaviour.
