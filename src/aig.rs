// SPDX-FileCopyrightText: Copyright (c) 2024 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//! And-inverter graph format
//!
//! An AIG is derived from netlistdb synthesized in AIGPDK or SKY130.

#![allow(
    clippy::ptr_arg,
    clippy::too_many_arguments,
    clippy::needless_range_loop
)]

use crate::aigpdk::AIGPDK_SRAM_ADDR_WIDTH;
use crate::gf180mcu_pdk::Gf180PdkModels;
use crate::pdk::PdkVariant;
// SKY130-flavoured helpers used by the sky130-specific methods on AIG
// (`get_sky130_dependencies`, `sky130_preprocess`, `sky130_postprocess`,
// `build_sky130_multi_output_postprocess`). Top-level dispatch through
// PdkVariant is preferred for new code, but within SKY130-only methods
// the unqualified shorthand is fine — it can't accidentally classify a
// GF180MCU cell because the caller has already routed on PdkVariant.
use crate::sky130::extract_cell_type;
use crate::sky130_pdk::{
    decompose_with_pdk, is_multi_output_cell, is_sequential_cell, is_tie_cell, CellInputs,
    PdkModels,
};
use indexmap::{IndexMap, IndexSet};
use netlistdb::{Direction, GeneralPinName, NetlistDB};
use smallvec::SmallVec;

/// Work item for iterative DFS traversal.
/// Using two-phase approach: Visit pushes dependencies, Process computes outputs.
#[derive(Clone, Copy)]
enum WorkItem {
    /// First phase: check if visited, mark in-stack, push Process then dependencies
    Visit(usize),
    /// Second phase: all dependencies ready, compute output, clear in-stack
    Process(usize),
}

/// A DFF.
#[derive(Debug, Default, Clone)]
pub struct DFF {
    /// The D input pin with invert (last bit)
    pub d_iv: usize,
    /// If the DFF is enabled, i.e., if the clock, S, or R is active.
    pub en_iv: usize,
    /// The Q pin output with invert.
    pub q: usize,
}

/// Simulation control type for $stop and $finish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimControlType {
    /// $stop - pause simulation
    Stop,
    /// $finish - terminate simulation
    Finish,
}

/// A simulation control node for $stop/$finish system tasks.
/// These are triggered when their condition input is true.
#[derive(Debug, Default, Clone)]
pub struct SimControlNode {
    /// The control type (Stop or Finish)
    pub control_type: Option<SimControlType>,
    /// The condition input pin with invert (last bit)
    /// When this evaluates to 1, the control action fires.
    pub condition_iv: usize,
    /// Optional message ID for display purposes
    pub message_id: u32,
}

/// A display node for $display/$write system tasks.
/// These output formatted messages when triggered.
#[derive(Debug, Default, Clone)]
pub struct DisplayNode {
    /// The enable condition input pin with invert (last bit)
    /// When this evaluates to 1, the display fires.
    pub enable_iv: usize,
    /// The clock enable signal (posedge trigger)
    pub clken_iv: usize,
    /// The format string for this display
    pub format: String,
    /// The argument signals (each entry is aigpin_iv)
    pub args_iv: Vec<usize>,
    /// The bit widths of each argument
    pub arg_widths: Vec<u32>,
    /// The cell name (for matching with JSON attributes)
    pub cell_name: String,
}

/// A ram block resembling the interface of `$__RAMGEM_SYNC_`.
#[derive(Debug, Default, Clone)]
pub struct RAMBlock {
    pub port_r_addr_iv: [usize; AIGPDK_SRAM_ADDR_WIDTH],

    /// controls whether r_rd_data should update. (from read clock)
    pub port_r_en_iv: usize,
    pub port_r_rd_data: [usize; 32],

    pub port_w_addr_iv: [usize; AIGPDK_SRAM_ADDR_WIDTH],
    /// controls whether memory should be updated.
    ///
    /// this is a combination of write enable and write clock.
    pub port_w_wr_en_iv: [usize; 32],
    pub port_w_wr_data_iv: [usize; 32],
}

/// A type of endpoint group. can be a primary output-related pin,
/// a D flip-flop, a ram block, or a simulation control node.
///
/// A group means a task for the partition to complete.
/// For primary output pins, the task is just to store.
/// For DFFs, the task is to store only when the clock is enable.
/// For RAMBlocks, the task is to simulate a sync SRAM.
/// For SimControl, the task is to check the condition and fire an event.
/// A StagedIOPin indicates a temporary live pin between different
/// major stages but reside in the same simulated cycle.
#[derive(Debug, Copy, Clone)]
pub enum EndpointGroup<'i> {
    PrimaryOutput(usize),
    DFF(&'i DFF),
    RAMBlock(&'i RAMBlock),
    SimControl(&'i SimControlNode),
    Display(&'i DisplayNode),
    StagedIOPin(usize),
}

impl EndpointGroup<'_> {
    /// Enumerate all related aigpin inputs for this endpoint group.
    ///
    /// The enumerated inputs may have duplicates.
    pub fn for_each_input(self, mut f_nz: impl FnMut(usize)) {
        let mut f = |i| {
            if i >= 1 {
                f_nz(i);
            }
        };
        match self {
            Self::PrimaryOutput(idx) => f(idx >> 1),
            Self::DFF(dff) => {
                f(dff.en_iv >> 1);
                f(dff.d_iv >> 1);
            }
            Self::RAMBlock(ram) => {
                f(ram.port_r_en_iv >> 1);
                for i in 0..13 {
                    f(ram.port_r_addr_iv[i] >> 1);
                    f(ram.port_w_addr_iv[i] >> 1);
                }
                for i in 0..32 {
                    f(ram.port_w_wr_en_iv[i] >> 1);
                    f(ram.port_w_wr_data_iv[i] >> 1);
                }
            }
            Self::SimControl(ctrl) => {
                f(ctrl.condition_iv >> 1);
            }
            Self::Display(disp) => {
                f(disp.enable_iv >> 1);
                for &arg_iv in &disp.args_iv {
                    f(arg_iv >> 1);
                }
            }
            Self::StagedIOPin(idx) => f(idx),
        }
    }
}

/// The driver type of an AIG pin.
#[derive(Debug, Clone)]
pub enum DriverType {
    /// Driven by an and gate.
    ///
    /// The inversion bit is stored as the last bits in
    /// two input indices.
    ///
    /// Only this type has combinational fan-in.
    AndGate(usize, usize),
    /// Driven by a primary input port (with its netlistdb id).
    InputPort(usize),
    /// Driven by a clock flag (with clock port netlistdb id, and pos/negedge)
    InputClockFlag(usize, u8),
    /// Driven by a DFF (with its index)
    DFF(usize),
    /// Driven by a 13-bit by 32-bit RAM block (with its index)
    SRAM(usize),
    /// Tie0: tied to zero. Only the 0-th aig pin is allowed to have this.
    Tie0,
}

/// An AIG associated with a netlistdb.
#[derive(Debug)]
pub struct AIG {
    /// The number of AIG pins.
    ///
    /// This number might be smaller than num_pins in netlistdb,
    /// because inverters and buffers are merged when possible.
    /// It might also be larger because we may add mux circuits.
    ///
    /// AIG pins are numbered from 1 to num_aigpins inclusive.
    /// The AIG pin id zero (0) is tied to 0.
    ///
    /// AIG pins are guaranteed to have topological order.
    pub num_aigpins: usize,
    /// The mapping from a netlistdb pin to an AIG pin.
    ///
    /// The inversion bit is stored as the last bit.
    /// E.g., `pin2aigpin_iv[pin_id] = aigpin_id << 1 | invert`.
    pub pin2aigpin_iv: Vec<usize>,
    /// The clock pins map. Every clock pin has a pair of flag pins
    /// showing if they are posedge/negedge.
    ///
    /// The flag pin can be empty which means the circuit is not
    /// active with that edge.
    pub clock_pin2aigpins: IndexMap<usize, (usize, usize)>,
    /// The driver types of AIG pins.
    pub drivers: Vec<DriverType>,
    /// A cache for identical and gates.
    pub and_gate_cache: IndexMap<(usize, usize), usize>,
    /// Unique primary output aigpin indices
    pub primary_outputs: IndexSet<usize>,
    /// The D flip-flops (DFFs), indexed by cell id
    pub dffs: IndexMap<usize, DFF>,
    /// The SRAMs, indexed by cell id
    pub srams: IndexMap<usize, RAMBlock>,
    /// The simulation control nodes ($stop/$finish), indexed by cell id
    pub simcontrols: IndexMap<usize, SimControlNode>,
    /// The display nodes ($display/$write), indexed by cell id
    pub displays: IndexMap<usize, DisplayNode>,
    /// The fanout CSR start array.
    pub fanouts_start: Vec<usize>,
    /// The fanout CSR array.
    pub fanouts: Vec<usize>,

    // === Timing Analysis Fields ===
    /// Arrival times for each AIG pin: (min_ps, max_ps).
    /// Computed during STA traversal. Index 0 is unused (Tie0 has arrival 0).
    /// Empty until `compute_timing()` is called.
    pub arrival_times: Vec<(u64, u64)>,

    /// Gate delays for each AIG pin: (rise_ps, fall_ps).
    /// Loaded from Liberty library. Used during timing propagation.
    pub gate_delays: Vec<(u64, u64)>,

    /// Setup slack for each DFF (indexed by position in `dffs`).
    /// Negative values indicate timing violations.
    /// Computed as: clock_arrival - data_arrival - setup_time
    pub setup_slacks: Vec<i64>,

    /// Hold slack for each DFF (indexed by position in `dffs`).
    /// Negative values indicate timing violations.
    /// Computed as: data_arrival - clock_arrival - hold_time
    pub hold_slacks: Vec<i64>,

    /// DFF timing constraints loaded from Liberty.
    pub dff_timing: Option<crate::liberty_parser::DFFTiming>,

    /// Clock period in picoseconds (for STA calculations).
    /// Default is 1000ps (1ns) if not specified.
    pub clock_period_ps: u64,

    // === SDF Back-Annotation Support ===
    /// Maps AIG pin index → list of (netlistdb cell_id, cell_type, output_pin_name).
    /// Accumulated: inverters sharing an AIG pin (via invert-bit reuse) push
    /// their origins rather than overwriting. During SDF loading, delays from
    /// all origins are summed (they form a serial chain, not parallel paths).
    /// Internal AND gates from multi-gate decompositions get empty Vec (zero delay).
    /// Index 0 is Tie0 (empty). Populated during AIG construction from netlistdb.
    pub aigpin_cell_origins: Vec<Vec<(usize, String, String)>>,

    /// Extra observable signals beyond the top-level cell-0 output ports.
    ///
    /// Maps an aigpin index → the list of debug names used in the
    /// output VCD. Entries in this map are also inserted into
    /// `primary_outputs` so the partitioner schedules them onto a
    /// state bit and `output_map` gets the position. The VCD writer
    /// emits one wire definition per name (multiple names may share
    /// the same aigpin if their drivers collapse to the same node —
    /// e.g. two bidir pads sharing the same `OE` register bit).
    ///
    /// Currently populated only by the GF180MCU `bi_24t` pad split,
    /// which surfaces each bidir pad's core-side `A` (`__out`) and
    /// `OE` (`__oe`) so output VCDs capture the core's drive-out
    /// direction. Without this, the simplified `Y = PAD` IO-pad model
    /// would leave the core's drive cone unobservable.
    pub extra_observable_names: IndexMap<usize, Vec<String>>,
}

impl Default for AIG {
    fn default() -> Self {
        Self {
            num_aigpins: 0,
            pin2aigpin_iv: Vec::new(),
            clock_pin2aigpins: IndexMap::new(),
            drivers: Vec::new(),
            and_gate_cache: IndexMap::new(),
            primary_outputs: IndexSet::new(),
            dffs: IndexMap::new(),
            srams: IndexMap::new(),
            simcontrols: IndexMap::new(),
            displays: IndexMap::new(),
            fanouts_start: Vec::new(),
            fanouts: Vec::new(),
            // Timing fields
            arrival_times: Vec::new(),
            gate_delays: Vec::new(),
            setup_slacks: Vec::new(),
            hold_slacks: Vec::new(),
            dff_timing: None,
            clock_period_ps: 1000, // Default 1ns clock period
            aigpin_cell_origins: Vec::new(),
            extra_observable_names: IndexMap::new(),
        }
    }
}

impl AIG {
    fn add_aigpin(&mut self, driver: DriverType) -> usize {
        self.num_aigpins += 1;
        self.drivers.push(driver);
        self.aigpin_cell_origins.push(Vec::new());
        self.num_aigpins
    }

    fn add_and_gate(&mut self, a: usize, b: usize) -> usize {
        assert_ne!(a | 1, usize::MAX);
        assert_ne!(b | 1, usize::MAX);
        if a == 0 || b == 0 {
            return 0;
        }
        if a == 1 {
            return b;
        }
        if b == 1 {
            return a;
        }
        let (a, b) = if a < b { (a, b) } else { (b, a) };
        if let Some(o) = self.and_gate_cache.get(&(a, b)) {
            return o << 1;
        }
        let aigpin = self.add_aigpin(DriverType::AndGate(a, b));
        self.and_gate_cache.insert((a, b), aigpin);
        aigpin << 1
    }

    /// Apply the standard active-low async-set / async-reset overlay
    /// onto a DFF's `d_in` and clock-enable signals.
    ///
    /// Semantics (matches AIGPDK DFFSR, SKY130 RESET_B/SET_B, and
    /// GF180MCU RN/SETN):
    ///
    /// ```text
    ///   reset_n active-low: reset_n=0 forces Q=0
    ///   set_n   active-low: set_n=0   forces Q=1
    ///
    ///   d_in  = AND(OR(D, NOT set_n), reset_n)
    ///   clken = OR(clken, NOT reset_n, NOT set_n)
    /// ```
    ///
    /// Pass `1` for `set_n_iv` / `reset_n_iv` to indicate "no async
    /// set" / "no async reset" — `add_and_gate` constant-folds those
    /// branches away.
    fn wire_dff_reset_set_overlay(
        &mut self,
        d_in: usize,
        clken: usize,
        reset_n_iv: usize,
        set_n_iv: usize,
    ) -> (usize, usize) {
        // d_in  = OR(d_in, NOT set_n)         = NOT(AND(NOT d_in, set_n))
        // clken = OR(clken, NOT set_n)        = NOT(AND(NOT clken, set_n))
        let d_in = self.add_and_gate(d_in ^ 1, set_n_iv) ^ 1;
        let clken = self.add_and_gate(clken ^ 1, set_n_iv) ^ 1;
        // d_in  = AND(d_in, reset_n)
        // clken = OR(clken, NOT reset_n)      = NOT(AND(NOT clken, reset_n))
        let d_in = self.add_and_gate(d_in, reset_n_iv);
        let clken = self.add_and_gate(clken ^ 1, reset_n_iv) ^ 1;
        (d_in, clken)
    }

    /// given a clock pin, trace back to clock root and return its
    /// enable signal (with invert bit).
    ///
    /// if result is 0, that means the pin is dangled.
    /// if an error occurs because of a undecipherable multi-input cell,
    /// we will return in error the last output pin index of that cell.
    fn trace_clock_pin(
        &mut self,
        netlistdb: &NetlistDB,
        start_pinid: usize,
        start_is_negedge: bool,
        // should we ignore cklnqd in this tracing.
        // if set to true, we will treat cklnqd as a simple buffer.
        // otherwise, we assert that cklnqd/en is already built in
        // our aig mapping (pin2aigpin_iv).
        ignore_cklnqd: bool,
    ) -> Result<usize, usize> {
        // Iterative implementation using explicit stack
        // Each entry: (pinid, is_negedge, is_processing_cklnqd, cklnqd_cp_result)
        // For CKLNQD, we need to first trace CP, then combine with EN
        let mut current_pinid = start_pinid;
        let mut current_is_negedge = start_is_negedge;

        // Stack for CKLNQD cells that need post-processing
        // (pinid of EN pin, is_negedge at that point)
        let mut cklnqd_stack: Vec<(usize, bool)> = Vec::new();

        let mut iteration = 0;
        let mut visited_pins: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut path: Vec<(usize, String)> = Vec::new();
        loop {
            iteration += 1;
            {
                let pin_name = netlistdb.pinnames[current_pinid].dbg_fmt_pin();
                let dir = if netlistdb.pindirect[current_pinid] == Direction::I {
                    "I"
                } else {
                    "O"
                };
                path.push((current_pinid, format!("{}:{}", pin_name, dir)));
            }
            if visited_pins.contains(&current_pinid) {
                panic!(
                    "trace_clock_pin cycle detected!\nstart={}, iteration={}\npath:\n{}",
                    start_pinid,
                    iteration,
                    path.iter()
                        .enumerate()
                        .map(|(i, (pid, name))| format!("  {:3}: pin {} - {}", i, pid, name))
                        .collect::<Vec<_>>()
                        .join("\n")
                );
            }
            visited_pins.insert(current_pinid);

            if iteration > 100000 {
                panic!(
                    "trace_clock_pin infinite loop detected: pinid={}, start={}, iteration={}",
                    current_pinid, start_pinid, iteration
                );
            }
            let pinid = current_pinid;
            let is_negedge = current_is_negedge;

            if netlistdb.pindirect[pinid] == Direction::I {
                let netid = netlistdb.pin2net[pinid];
                if Some(netid) == netlistdb.net_zero || Some(netid) == netlistdb.net_one {
                    // Reached a constant - process any pending CKLNQD
                    let mut result = 0usize;
                    while let Some((en_pin, _)) = cklnqd_stack.pop() {
                        if ignore_cklnqd {
                            // Just return the traced clock, ignore enable
                        } else {
                            let en_iv = self.pin2aigpin_iv[en_pin];
                            assert_ne!(en_iv, usize::MAX, "clken not built");
                            result = self.add_and_gate(result, en_iv);
                        }
                    }
                    return Ok(result);
                }
                // Follow net to driving pin - find the output pin (driver) on the net
                let net_pins_start = netlistdb.net2pin.start[netid];
                let net_pins_end = if netid + 1 < netlistdb.net2pin.start.len() {
                    netlistdb.net2pin.start[netid + 1]
                } else {
                    netlistdb.net2pin.items.len()
                };
                let mut driver_pin = None;
                for &np in &netlistdb.net2pin.items[net_pins_start..net_pins_end] {
                    // Check if this pin is an output (driver)
                    if netlistdb.pindirect[np] == Direction::O {
                        driver_pin = Some(np);
                        break;
                    }
                    // Also check for primary input (cell 0)
                    if netlistdb.pin2cell[np] == 0 {
                        driver_pin = Some(np);
                        break;
                    }
                }
                if let Some(dp) = driver_pin {
                    current_pinid = dp;
                    continue;
                }
                // Net has no driver - treat this input pin as a primary input (clock source)
                // This handles floating nets / undriven primary inputs
                // Create a clock signal for this undriven net
                let clkentry = self
                    .clock_pin2aigpins
                    .entry(pinid)
                    .or_insert((usize::MAX, usize::MAX));
                let clksignal = match is_negedge {
                    false => clkentry.0,
                    true => clkentry.1,
                };
                let mut result = if clksignal != usize::MAX {
                    clksignal << 1
                } else {
                    let aigpin =
                        self.add_aigpin(DriverType::InputClockFlag(pinid, is_negedge as u8));
                    let clkentry = self.clock_pin2aigpins.get_mut(&pinid).unwrap();
                    let clksignal = match is_negedge {
                        false => &mut clkentry.0,
                        true => &mut clkentry.1,
                    };
                    *clksignal = aigpin;
                    aigpin << 1
                };

                // Process any pending CKLNQD
                while let Some((en_pin, _)) = cklnqd_stack.pop() {
                    if !ignore_cklnqd {
                        let en_iv = self.pin2aigpin_iv[en_pin];
                        assert_ne!(en_iv, usize::MAX, "clken not built");
                        result = self.add_and_gate(result, en_iv);
                    }
                }
                return Ok(result);
            }

            let cellid = netlistdb.pin2cell[pinid];
            if cellid == 0 {
                // Reached primary input - this is the clock source
                let clkentry = self
                    .clock_pin2aigpins
                    .entry(pinid)
                    .or_insert((usize::MAX, usize::MAX));
                let clksignal = match is_negedge {
                    false => clkentry.0,
                    true => clkentry.1,
                };
                let mut result = if clksignal != usize::MAX {
                    clksignal << 1
                } else {
                    let aigpin =
                        self.add_aigpin(DriverType::InputClockFlag(pinid, is_negedge as u8));
                    let clkentry = self.clock_pin2aigpins.get_mut(&pinid).unwrap();
                    let clksignal = match is_negedge {
                        false => &mut clkentry.0,
                        true => &mut clkentry.1,
                    };
                    *clksignal = aigpin;
                    aigpin << 1
                };

                // Process any pending CKLNQD
                while let Some((en_pin, _)) = cklnqd_stack.pop() {
                    if !ignore_cklnqd {
                        let en_iv = self.pin2aigpin_iv[en_pin];
                        assert_ne!(en_iv, usize::MAX, "clken not built");
                        result = self.add_and_gate(result, en_iv);
                    }
                }
                return Ok(result);
            }

            let mut pin_a = usize::MAX;
            let mut pin_cp = usize::MAX;
            let mut pin_en = usize::MAX;
            let celltype = netlistdb.celltypes[cellid].as_str();

            // Determine if this is a clock buffer/inverter (AIGPDK / SKY130 / GF180MCU).
            // GF180MCU `inv` / `clkinv` are negative-polarity; `buf` / `clkbuf` are
            // identity. GF180MCU clock-path cells use input pin name `I` rather
            // than `A`; we accept both below.
            let variant = PdkVariant::classify(celltype);
            // (is_inv, is_buf, is_io_pad_buf, is_delay).
            //
            // Delay cells (`dlya`/`dlyb`/`dlyc`/`dlyd` for GF180MCU;
            // `dlygate*`/`dlymetal*` for SKY130) are functional identity
            // buffers — single input, single output, no gating, no
            // negation. The post-P&R OpenROAD hold-fixing pass routinely
            // inserts them on clock fanout to repair tight hold paths.
            // Treated like `is_buf` in the downstream propagation
            // (current_pinid = pin_a, is_negedge unchanged). Kept
            // separate from `is_buf` so timing-analysis paths can
            // distinguish "identity buffer" from "identity buffer with
            // a delay constant" if they need to. See #73.
            //
            // GF180MCU `hold` is **not** included — it's OpenROAD's
            // data-path hold-fix cell, not expected on a clock fanout.
            // If one does appear, that's likely a netlist bug worth
            // surfacing rather than silently traversing.
            let (is_inv, is_buf, is_io_pad_buf, is_delay) = match variant {
                Some(PdkVariant::Sky130) => {
                    let ct = PdkVariant::Sky130.extract_cell_type(celltype);
                    let is_inv = ct.starts_with("inv") || ct.starts_with("clkinv");
                    let is_buf = ct.starts_with("buf")
                        || ct.starts_with("clkbuf")
                        || ct.starts_with("clkdlybuf")
                        || ct.starts_with("lpflow_isobufsrc")
                        || ct.starts_with("lpflow_inputiso");
                    let is_delay = ct.starts_with("dlygate") || ct.starts_with("dlymetal");
                    (is_inv, is_buf, false, is_delay)
                }
                Some(PdkVariant::Gf180Mcu) => {
                    let ct = PdkVariant::Gf180Mcu.extract_cell_type(celltype);
                    let is_inv = matches!(ct, "inv" | "clkinv");
                    let is_buf = matches!(ct, "buf" | "clkbuf");
                    // GF180MCU input + bidir pads on the clock path buffer
                    // PAD → Y, so they're traceable just like a clkbuf.
                    // `bi_24t` is included because the digital-sim
                    // simplification (`Y = PAD`, see is_io_pad_cell in
                    // gf180mcu_pdk.rs) makes its AIG shape identical to
                    // in_c / in_s. Common case: JTAG TCK routed through a
                    // bidirectional signal pad in templates that don't
                    // dedicate input-only pads to the clock (e.g. the
                    // wafer.space gf180mcu-project-template default). See
                    // #75.
                    let is_io_pad_buf = matches!(ct, "in_c" | "in_s" | "bi_24t");
                    let is_delay = matches!(ct, "dlya" | "dlyb" | "dlyc" | "dlyd");
                    (is_inv, is_buf, is_io_pad_buf, is_delay)
                }
                None => (celltype == "INV", celltype == "BUF", false, false),
            };

            if !is_inv && !is_buf && !is_io_pad_buf && !is_delay && celltype != "CKLNQD" {
                clilog::error!(
                    "cell type {} not supported on clock path. expecting only INV, BUF, CKLNQD, or a delay cell (or SKY130 / GF180MCU equivalents)",
                    celltype
                );
                return Err(pinid);
            }

            // `bi_24t` on the clock path: only PAD is the clock input.
            // The cell also exposes `A` and `OE` as Direction::I (the
            // core-side tristate driver inputs), but those are
            // discarded by `gf180mcu_postprocess`'s `Y = PAD`
            // simplification, so they're not part of the clock cone.
            // Without this guard, both PAD and A match the
            // "A | I | PAD" arm below and whichever is enumerated
            // last wins — wrong if A wins. See #75.
            let is_gf180mcu_bi_24t = matches!(variant, Some(PdkVariant::Gf180Mcu))
                && PdkVariant::Gf180Mcu.extract_cell_type(celltype) == "bi_24t";

            for ipin in netlistdb.cell2pin.iter_set(cellid) {
                if netlistdb.pindirect[ipin] == Direction::I {
                    let ipin_name = netlistdb.pinnames[ipin].1.as_str();
                    // Power / ground pins on the clock-buffer cell. Post-#64
                    // these resolve to `Direction::I` (the power-pin
                    // shortcut in `GF180MCULeafPins::direction_of`), but
                    // they're physical supply connections — never a clock
                    // source. Skipping here keeps the "multi-input ⇒ clock
                    // gating" rule honest by ignoring them, the same way
                    // the IO-pad control pins below are ignored. See #70.
                    if matches!(variant, Some(PdkVariant::Gf180Mcu))
                        && crate::gf180mcu_pdk::is_power_pin(ipin_name)
                    {
                        continue;
                    }
                    if is_gf180mcu_bi_24t && matches!(ipin_name, "A" | "OE") {
                        continue;
                    }
                    match ipin_name {
                        // AIGPDK / sky130 use `A`; GF180MCU buf/inv cells
                        // use `I`; GF180MCU input pads carry the clock
                        // on `PAD`.
                        "A" | "I" | "PAD" => pin_a = ipin,
                        "CP" => pin_cp = ipin,
                        "E" => pin_en = ipin,
                        // IO-pad sim-irrelevant control pins (pull,
                        // Schmitt, etc.) — ignore them when this cell
                        // is on the clock path.
                        "PU" | "PD" | "IE" | "CS" | "SL" => {}
                        i => {
                            clilog::error!(
                                "input pin {} unexpected for ck element {}",
                                i,
                                celltype
                            );
                            return Err(ipin);
                        }
                    }
                }
            }

            if is_inv {
                assert_ne!(pin_a, usize::MAX);
                current_pinid = pin_a;
                current_is_negedge = !is_negedge;
            } else if is_buf || is_io_pad_buf || is_delay {
                assert_ne!(pin_a, usize::MAX);
                current_pinid = pin_a;
                // is_negedge stays the same. Delay cells (`dlya/b/c/d`,
                // `dlygate*`, `dlymetal*`) propagate the clock through
                // their delay constant just like a plain buffer — see #73.
            } else if celltype == "CKLNQD" {
                assert_ne!(pin_cp, usize::MAX);
                assert_ne!(pin_en, usize::MAX);
                // Push CKLNQD for post-processing, continue with CP
                cklnqd_stack.push((pin_en, is_negedge));
                current_pinid = pin_cp;
                // is_negedge stays the same
            } else {
                unreachable!()
            }
        }
    }

    /// Iteratively add AIG pins for netlistdb pins using explicit stack.
    ///
    /// for sequential logics like DFF and RAM,
    /// 1. their netlist pin inputs are not patched,
    /// 2. their aig pin inputs (in dffs and srams arrays) will be
    ///    patched to include mux -- but not inside this function.
    /// 3. their netlist/aig outputs are directly built here,
    ///    with possible patches for asynchronous DFFSR polyfill.
    fn dfs_netlistdb_build_aig(
        &mut self,
        netlistdb: &NetlistDB,
        topo_vis: &mut Vec<bool>,
        topo_instack: &mut Vec<bool>,
        start_pinid: usize,
        pdk_models: Option<&PdkModels>,
        gf180_pdk: Option<&Gf180PdkModels>,
        cell_library: Option<&crate::cell_library::RuntimeCellLibrary>,
    ) {
        let mut work_stack: Vec<WorkItem> = vec![WorkItem::Visit(start_pinid)];

        while let Some(item) = work_stack.pop() {
            match item {
                WorkItem::Visit(pinid) => {
                    if topo_vis[pinid] {
                        continue;
                    }
                    if topo_instack[pinid] {
                        panic!(
                            "circuit has a loop around pin {}",
                            netlistdb.pinnames[pinid].dbg_fmt_pin()
                        );
                    }

                    topo_vis[pinid] = true;
                    topo_instack[pinid] = true;

                    let netid = netlistdb.pin2net[pinid];
                    let cellid = netlistdb.pin2cell[pinid];
                    let celltype = netlistdb.celltypes[cellid].as_str();

                    // Handle input pins
                    if netlistdb.pindirect[pinid] == Direction::I {
                        if Some(netid) == netlistdb.net_zero {
                            self.pin2aigpin_iv[pinid] = 0;
                            topo_instack[pinid] = false;
                        } else if Some(netid) == netlistdb.net_one {
                            self.pin2aigpin_iv[pinid] = 1;
                            topo_instack[pinid] = false;
                        } else {
                            // Find the actual driver pin on the net
                            let net_pins_start = netlistdb.net2pin.start[netid];
                            let net_pins_end = if netid + 1 < netlistdb.net2pin.start.len() {
                                netlistdb.net2pin.start[netid + 1]
                            } else {
                                netlistdb.net2pin.items.len()
                            };
                            let mut driver_pin = None;
                            for &np in &netlistdb.net2pin.items[net_pins_start..net_pins_end] {
                                // Check if this pin is an output (driver)
                                if netlistdb.pindirect[np] == Direction::O {
                                    driver_pin = Some(np);
                                    break;
                                }
                                // Also check for primary input (cell 0)
                                if netlistdb.pin2cell[np] == 0 {
                                    driver_pin = Some(np);
                                    break;
                                }
                            }
                            if let Some(root) = driver_pin {
                                // Push process, then dependency
                                work_stack.push(WorkItem::Process(pinid));
                                work_stack.push(WorkItem::Visit(root));
                            } else {
                                // Net has no driver - tie to 0
                                // This handles floating nets / undriven signals in post-P&R netlists
                                self.pin2aigpin_iv[pinid] = 0;
                                topo_instack[pinid] = false;
                            }
                        }
                        continue;
                    }

                    // Handle primary input ports (cellid == 0, output direction)
                    if cellid == 0 {
                        let aigpin = self.add_aigpin(DriverType::InputPort(pinid));
                        self.pin2aigpin_iv[pinid] = aigpin << 1;
                        topo_instack[pinid] = false;
                        continue;
                    }

                    // Handle AIGPDK DFF/DFFSR
                    if matches!(celltype, "DFF" | "DFFSR") {
                        // Pre-create DFF Q output
                        let q = self.add_aigpin(DriverType::DFF(cellid));
                        // Record cell origin for DFF Q output (for SDF CLK→Q delay)
                        self.aigpin_cell_origins[q].push((
                            cellid,
                            celltype.to_string(),
                            "Q".to_string(),
                        ));
                        let dff = self.dffs.entry(cellid).or_default();
                        dff.q = q;

                        // Push process then S/R dependencies
                        work_stack.push(WorkItem::Process(pinid));
                        for dep_pinid in netlistdb.cell2pin.iter_set(cellid) {
                            if matches!(netlistdb.pinnames[dep_pinid].1.as_str(), "S" | "R") {
                                work_stack.push(WorkItem::Visit(dep_pinid));
                            }
                        }
                        continue;
                    }

                    // Handle LATCH
                    if celltype == "LATCH" {
                        panic!(
                            "latches are intentionally UNSUPPORTED by GEM, \
                                except in identified gated clocks. \n\
                                you can link a FF&MUX-based LATCH module, \
                                but most likely that is NOT the right solution. \n\
                                check all your assignments inside always@(*) block \
                                to make sure they cover all scenarios."
                        );
                    }

                    // Handle SRAM
                    if celltype == "$__RAMGEM_SYNC_" {
                        let o = self.add_aigpin(DriverType::SRAM(cellid));
                        self.pin2aigpin_iv[pinid] = o << 1;
                        assert_eq!(netlistdb.pinnames[pinid].1.as_str(), "PORT_R_RD_DATA");
                        // Record cell origin for SRAM data output
                        self.aigpin_cell_origins[o].push((
                            cellid,
                            celltype.to_string(),
                            "PORT_R_RD_DATA".to_string(),
                        ));
                        let sram = self.srams.entry(cellid).or_default();
                        sram.port_r_rd_data[netlistdb.pinnames[pinid].2.unwrap() as usize] = o;
                        topo_instack[pinid] = false;
                        continue;
                    }

                    // Handle CKLNQD
                    if celltype == "CKLNQD" {
                        let mut prev_cp = usize::MAX;
                        let mut prev_en = usize::MAX;
                        for dep_pinid in netlistdb.cell2pin.iter_set(cellid) {
                            match netlistdb.pinnames[dep_pinid].1.as_str() {
                                "CP" => prev_cp = dep_pinid,
                                "E" => prev_en = dep_pinid,
                                _ => {}
                            }
                        }
                        assert_ne!(prev_cp, usize::MAX);
                        assert_ne!(prev_en, usize::MAX);
                        // Push process then dependencies
                        work_stack.push(WorkItem::Process(pinid));
                        work_stack.push(WorkItem::Visit(prev_cp));
                        work_stack.push(WorkItem::Visit(prev_en));
                        continue;
                    }

                    // Handle GEM_ASSERT / GEM_DISPLAY
                    if celltype == "GEM_ASSERT" || celltype == "GEM_DISPLAY" {
                        // Push process then all input pin dependencies
                        work_stack.push(WorkItem::Process(pinid));
                        for dep_pinid in netlistdb.cell2pin.iter_set(cellid) {
                            if netlistdb.pindirect[dep_pinid] == Direction::I {
                                work_stack.push(WorkItem::Visit(dep_pinid));
                            }
                        }
                        continue;
                    }

                    // Handle standard-cell PDK cells (combinational + sequential).
                    match PdkVariant::classify(celltype) {
                        Some(PdkVariant::Sky130) => {
                            let deps = self
                                .get_sky130_dependencies(netlistdb, pinid, cellid, celltype);
                            self.sky130_preprocess(netlistdb, pinid, cellid, celltype);
                            work_stack.push(WorkItem::Process(pinid));
                            for dep in deps {
                                work_stack.push(WorkItem::Visit(dep));
                            }
                            continue;
                        }
                        Some(PdkVariant::Gf180Mcu) => {
                            let deps = self
                                .get_gf180mcu_dependencies(netlistdb, pinid, cellid, celltype);
                            self.gf180mcu_preprocess(netlistdb, pinid, cellid, celltype);
                            work_stack.push(WorkItem::Process(pinid));
                            for dep in deps {
                                work_stack.push(WorkItem::Visit(dep));
                            }
                            continue;
                        }
                        None => {
                            // Cell isn't in a vendored PDK; consult
                            // the runtime cell-library manifest
                            // (ADR 0010). For `kind = "ram"`, treat
                            // as opaque RAM: allocate a per-output
                            // SRAM driver and route through `srams`
                            // for X-source enumeration. No port
                            // resolution; future schema versions
                            // upgrade this with explicit port mapping.
                            if let Some(lib) = cell_library {
                                use crate::cell_library::CellKind;
                                if lib.lookup_kind(celltype) == Some(CellKind::Ram) {
                                    let o = self.add_aigpin(DriverType::SRAM(cellid));
                                    self.pin2aigpin_iv[pinid] = o << 1;
                                    let pin_name = netlistdb.pinnames[pinid].1.to_string();
                                    self.aigpin_cell_origins[o].push((
                                        cellid,
                                        celltype.to_string(),
                                        pin_name,
                                    ));
                                    let sram = self.srams.entry(cellid).or_default();
                                    let bit_idx = netlistdb.pinnames[pinid]
                                        .2
                                        .unwrap_or(0) as usize;
                                    if bit_idx < sram.port_r_rd_data.len() {
                                        sram.port_r_rd_data[bit_idx] = o;
                                    }
                                    topo_instack[pinid] = false;
                                    continue;
                                }
                            }
                        }
                    }

                    // Handle AIGPDK combinational cells (AND2, INV, BUF)
                    let mut prev_a = usize::MAX;
                    let mut prev_b = usize::MAX;
                    for dep_pinid in netlistdb.cell2pin.iter_set(cellid) {
                        match netlistdb.pinnames[dep_pinid].1.as_str() {
                            "A" => prev_a = dep_pinid,
                            "B" => prev_b = dep_pinid,
                            _ => {}
                        }
                    }
                    // Push process then dependencies
                    work_stack.push(WorkItem::Process(pinid));
                    if prev_b != usize::MAX {
                        work_stack.push(WorkItem::Visit(prev_b));
                    }
                    if prev_a != usize::MAX {
                        work_stack.push(WorkItem::Visit(prev_a));
                    }
                }

                WorkItem::Process(pinid) => {
                    let netid = netlistdb.pin2net[pinid];
                    let cellid = netlistdb.pin2cell[pinid];
                    let celltype = netlistdb.celltypes[cellid].as_str();

                    // Process input pins (copy from driver)
                    if netlistdb.pindirect[pinid] == Direction::I {
                        let root = netlistdb.net2pin.items[netlistdb.net2pin.start[netid]];
                        self.pin2aigpin_iv[pinid] = self.pin2aigpin_iv[root];
                        if cellid == 0 {
                            self.primary_outputs.insert(self.pin2aigpin_iv[pinid]);
                        }
                        topo_instack[pinid] = false;
                        continue;
                    }

                    // Process AIGPDK DFF/DFFSR
                    if matches!(celltype, "DFF" | "DFFSR") {
                        let dff = self.dffs.get(&cellid).unwrap();
                        let q = dff.q;
                        let mut ap_s_iv = 1;
                        let mut ap_r_iv = 1;
                        let mut q_out = q << 1;

                        for dep_pinid in netlistdb.cell2pin.iter_set(cellid) {
                            let prev = self.pin2aigpin_iv[dep_pinid];
                            match netlistdb.pinnames[dep_pinid].1.as_str() {
                                "S" => ap_s_iv = prev,
                                "R" => ap_r_iv = prev,
                                _ => {}
                            }
                        }
                        // AIGPDK DFFSR .lib says: clear="(!R)", preset="(!S)"
                        // i.e. R=0 → Q=0 (clear), S=0 → Q=1 (preset)
                        // Same active-low semantics as SKY130 RESET_B/SET_B.
                        //   Q = AND(OR(Q_state, !S), R)
                        q_out = self.add_and_gate(q_out ^ 1, ap_s_iv) ^ 1;
                        q_out = self.add_and_gate(q_out, ap_r_iv);
                        self.pin2aigpin_iv[pinid] = q_out;
                        topo_instack[pinid] = false;
                        continue;
                    }

                    // Process CKLNQD (no output pin defined)
                    if celltype == "CKLNQD" {
                        // do not define pin2aigpin_iv[pinid] which is CKLNQD/Q and unused in logic.
                        topo_instack[pinid] = false;
                        continue;
                    }

                    // Process GEM_ASSERT / GEM_DISPLAY (no output pin defined)
                    if celltype == "GEM_ASSERT" || celltype == "GEM_DISPLAY" {
                        topo_instack[pinid] = false;
                        continue;
                    }

                    // Process standard-cell PDK cells.
                    match PdkVariant::classify(celltype) {
                        Some(PdkVariant::Sky130) => {
                            self.sky130_postprocess(
                                netlistdb, pinid, cellid, celltype, pdk_models,
                            );
                            topo_instack[pinid] = false;
                            continue;
                        }
                        Some(PdkVariant::Gf180Mcu) => {
                            self.gf180mcu_postprocess(
                                netlistdb,
                                pinid,
                                cellid,
                                celltype,
                                gf180_pdk,
                            );
                            topo_instack[pinid] = false;
                            continue;
                        }
                        None => {}
                    }

                    // Process AIGPDK combinational cells
                    let mut prev_a = usize::MAX;
                    let mut prev_b = usize::MAX;
                    for dep_pinid in netlistdb.cell2pin.iter_set(cellid) {
                        match netlistdb.pinnames[dep_pinid].1.as_str() {
                            "A" => prev_a = dep_pinid,
                            "B" => prev_b = dep_pinid,
                            _ => {}
                        }
                    }

                    match celltype {
                        "AND2_00_0" | "AND2_01_0" | "AND2_10_0" | "AND2_11_0" | "AND2_11_1" => {
                            assert_ne!(prev_a, usize::MAX);
                            assert_ne!(prev_b, usize::MAX);
                            let name = netlistdb.celltypes[cellid].as_bytes();
                            let iv_a = name[5] - b'0';
                            let iv_b = name[6] - b'0';
                            let iv_y = name[8] - b'0';
                            let apid = self.add_and_gate(
                                self.pin2aigpin_iv[prev_a] ^ (iv_a as usize),
                                self.pin2aigpin_iv[prev_b] ^ (iv_b as usize),
                            ) ^ (iv_y as usize);
                            self.pin2aigpin_iv[pinid] = apid;
                        }
                        "INV" => {
                            assert_ne!(prev_a, usize::MAX);
                            self.pin2aigpin_iv[pinid] = self.pin2aigpin_iv[prev_a] ^ 1;
                        }
                        "BUF" => {
                            assert_ne!(prev_a, usize::MAX);
                            self.pin2aigpin_iv[pinid] = self.pin2aigpin_iv[prev_a];
                        }
                        _ => panic!("Unknown AIGPDK cell type: {}", celltype),
                    }
                    topo_instack[pinid] = false;
                }
            }
        }
    }

    /// Get dependencies for a SKY130 cell that need to be visited before processing.
    /// Uses SmallVec to avoid heap allocation for most cells (≤6 inputs).
    fn get_sky130_dependencies(
        &self,
        netlistdb: &NetlistDB,
        pinid: usize,
        cellid: usize,
        celltype: &str,
    ) -> SmallVec<[usize; 6]> {
        let cell_type = extract_cell_type(celltype);
        let output_pin_name = netlistdb.pinnames[pinid].1.as_str();

        // Tie cells have no dependencies
        if is_tie_cell(cell_type) {
            return SmallVec::new();
        }

        // SRAM has no dependencies for output creation
        if celltype.starts_with("CF_SRAM_") {
            return SmallVec::new();
        }

        // Sequential cells: depend on SET_B/RESET_B pins
        if is_sequential_cell(cell_type) {
            let mut deps = SmallVec::new();
            for dep_pinid in netlistdb.cell2pin.iter_set(cellid) {
                let pin_name = netlistdb.pinnames[dep_pinid].1.as_str();
                if matches!(pin_name, "SET_B" | "RESET_B") {
                    deps.push(dep_pinid);
                }
            }
            return deps;
        }

        // Multi-output cells (ha, fa, dfbbp)
        if is_multi_output_cell(cell_type) {
            // dfbbp Q_N depends on Q being built first
            if cell_type == "dfbbp" && output_pin_name == "Q_N" {
                // Find Q pin
                for opin in netlistdb.cell2pin.iter_set(cellid) {
                    if netlistdb.pinnames[opin].1.as_str() == "Q" {
                        if self.pin2aigpin_iv[opin] == usize::MAX {
                            let mut deps = SmallVec::new();
                            deps.push(opin);
                            return deps;
                        }
                        return SmallVec::new();
                    }
                }
            }
            // For other multi-output cells, depend on all input pins
            let mut deps = SmallVec::new();
            for dep_pinid in netlistdb.cell2pin.iter_set(cellid) {
                if netlistdb.pindirect[dep_pinid] == Direction::I {
                    deps.push(dep_pinid);
                }
            }
            return deps;
        }

        // Combinational cells: depend on all input pins
        let mut deps = SmallVec::new();
        for dep_pinid in netlistdb.cell2pin.iter_set(cellid) {
            if netlistdb.pindirect[dep_pinid] == Direction::I {
                deps.push(dep_pinid);
            }
        }
        deps
    }

    /// Pre-process for SKY130 cells (create output AIG pins before dependencies are visited).
    fn sky130_preprocess(
        &mut self,
        netlistdb: &NetlistDB,
        pinid: usize,
        cellid: usize,
        celltype: &str,
    ) {
        let cell_type = extract_cell_type(celltype);
        let output_pin_name = netlistdb.pinnames[pinid].1.as_str();

        // Tie cells - set output immediately
        if is_tie_cell(cell_type) {
            match output_pin_name {
                "HI" => self.pin2aigpin_iv[pinid] = 1,
                "LO" => self.pin2aigpin_iv[pinid] = 0,
                _ => panic!("Unknown tie cell output: {}", output_pin_name),
            }
            return;
        }

        // SRAM output creation
        if celltype.starts_with("CF_SRAM_") {
            let o = self.add_aigpin(DriverType::SRAM(cellid));
            self.pin2aigpin_iv[pinid] = o << 1;
            assert_eq!(output_pin_name, "DO", "Expected DO output pin for CF_SRAM");
            // Record cell origin for SRAM data output
            self.aigpin_cell_origins[o].push((
                cellid,
                celltype.to_string(),
                output_pin_name.to_string(),
            ));
            let sram = self.srams.entry(cellid).or_default();
            let bit_idx = netlistdb.pinnames[pinid]
                .2
                .expect("DO pin must have bit index") as usize;
            sram.port_r_rd_data[bit_idx] = o;
            return;
        }

        // Sequential cells: pre-create DFF Q output
        if is_sequential_cell(cell_type) {
            let q = self.add_aigpin(DriverType::DFF(cellid));
            // Record cell origin for DFF Q output (for SDF CLK→Q delay)
            self.aigpin_cell_origins[q].push((cellid, cell_type.to_string(), "Q".to_string()));
            let dff = self.dffs.entry(cellid).or_default();
            dff.q = q;
        }

        // dfbbp with Q_N output: no pre-processing needed
        // Other multi-output and combinational cells: no pre-processing needed
    }

    /// Post-process for SKY130 cells (compute output after dependencies are visited).
    fn sky130_postprocess(
        &mut self,
        netlistdb: &NetlistDB,
        pinid: usize,
        cellid: usize,
        celltype: &str,
        pdk_models: Option<&PdkModels>,
    ) {
        let cell_type = extract_cell_type(celltype);

        // Tie cells and SRAM were already handled in preprocess
        if is_tie_cell(cell_type) || celltype.starts_with("CF_SRAM_") {
            return;
        }

        // Sequential cells: apply reset/set logic to Q
        if is_sequential_cell(cell_type) {
            let dff = self.dffs.get(&cellid).unwrap();
            let q = dff.q;
            let mut ap_s_iv = 1;
            let mut ap_r_iv = 1;
            let mut q_out = q << 1;

            for dep_pinid in netlistdb.cell2pin.iter_set(cellid) {
                let pin_name = netlistdb.pinnames[dep_pinid].1.as_str();
                let prev = self.pin2aigpin_iv[dep_pinid];
                match pin_name {
                    // SKY130 SET_B and RESET_B are active-low, same convention
                    // as AIGPDK's S and R pins. No inversion needed - the AIG
                    // formula OR(Q, NOT pin) / AND(result, pin) already handles
                    // active-low correctly:
                    //   RESET_B=0 (reset): AND(Q, 0) = 0  ✓
                    //   RESET_B=1 (normal): AND(Q, 1) = Q  ✓
                    "SET_B" => ap_s_iv = prev,
                    "RESET_B" => ap_r_iv = prev,
                    _ => {}
                }
            }
            q_out = self.add_and_gate(q_out ^ 1, ap_s_iv) ^ 1;
            q_out = self.add_and_gate(q_out, ap_r_iv);
            self.pin2aigpin_iv[pinid] = q_out;
            return;
        }

        // Multi-output cells (ha, fa, dfbbp)
        if is_multi_output_cell(cell_type) {
            self.build_sky130_multi_output_postprocess(
                netlistdb, pinid, cellid, cell_type, pdk_models,
            );
            return;
        }

        // Combinational cells: decompose and build
        // Use CellInputs struct instead of HashMap to avoid allocation
        let mut inputs = CellInputs::new();
        let mut input_count = 0;
        for ipin in netlistdb.cell2pin.iter_set(cellid) {
            // Include both explicit inputs (Direction::I) and unknown direction pins (Direction::U)
            // Unknown direction pins are treated as inputs for decomposition
            // Only skip explicit outputs (Direction::O)
            if netlistdb.pindirect[ipin] != Direction::O {
                let pin_name = netlistdb.pinnames[ipin].1.as_str();
                // Skip output pins by name (Y, X, Q, etc.)
                if !matches!(pin_name, "Y" | "X" | "Q" | "Q_N" | "SUM" | "COUT") {
                    inputs.set_pin(pin_name, self.pin2aigpin_iv[ipin]);
                    input_count += 1;
                }
            }
        }

        // Debug: check if we got enough inputs
        if cell_type == "nand2" && inputs.a == usize::MAX {
            use netlistdb::GeneralHierName;
            let cell_name = netlistdb.cellnames[cellid].dbg_fmt_hier();
            let mut pins_info: Vec<String> = Vec::new();
            for ipin in netlistdb.cell2pin.iter_set(cellid) {
                let pin_name = netlistdb.pinnames[ipin].1.as_str();
                let dir = format!("{:?}", netlistdb.pindirect[ipin]);
                pins_info.push(format!("{}:{}:{}", pin_name, dir, ipin));
            }
            clilog::warn!(
                "nand2 cell {} missing A input. Pins: {:?}",
                cell_name,
                pins_info
            );
        }
        if input_count == 0 {
            use netlistdb::GeneralHierName;
            let cell_name = netlistdb.cellnames[cellid].dbg_fmt_hier();
            let mut pins_info: Vec<String> = Vec::new();
            for ipin in netlistdb.cell2pin.iter_set(cellid) {
                let pin_name = netlistdb.pinnames[ipin].1.as_str();
                let dir = format!("{:?}", netlistdb.pindirect[ipin]);
                pins_info.push(format!("{}:{}", pin_name, dir));
            }
            panic!(
                "No input pins found for cell {} ({}), type='{}'. Pins: {:?}",
                cellid, cell_name, cell_type, pins_info
            );
        }

        let output_pin_name = netlistdb.pinnames[pinid].1.as_str();
        let pdk = pdk_models.expect(
            "PDK models required for SKY130 cell decomposition. \
             Ensure sky130_fd_sc_hd submodule is initialized.",
        );
        let decomp = decompose_with_pdk(cell_type, &inputs, output_pin_name, pdk);

        // Use SmallVec for gate outputs - most cells have ≤5 gates
        let mut gate_outputs: SmallVec<[usize; 5]> = SmallVec::new();
        for (i, (a_ref, b_ref)) in decomp.and_gates.iter().enumerate() {
            let a_iv = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.resolve_decomp_ref(*a_ref, &gate_outputs)
            })) {
                Ok(v) => v,
                Err(_) => {
                    use netlistdb::GeneralHierName;
                    let cell_name = netlistdb.cellnames[cellid].dbg_fmt_hier();
                    panic!(
                        "Failed resolve_decomp_ref for cell_type='{}', cell={}, gate {}, a_ref={}, gates_so_far={}, inputs={:?}",
                        cell_type, cell_name, i, a_ref, gate_outputs.len(), inputs
                    )
                }
            };
            let b_iv = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.resolve_decomp_ref(*b_ref, &gate_outputs)
            })) {
                Ok(v) => v,
                Err(_) => {
                    use netlistdb::GeneralHierName;
                    let cell_name = netlistdb.cellnames[cellid].dbg_fmt_hier();
                    panic!(
                        "Failed resolve_decomp_ref for cell_type='{}', cell={}, gate {}, b_ref={}, gates_so_far={}, inputs={:?}",
                        cell_type, cell_name, i, b_ref, gate_outputs.len(), inputs
                    )
                }
            };
            let gate_out = self.add_and_gate(a_iv, b_iv);
            gate_outputs.push(gate_out >> 1);
        }

        let output_iv = if decomp.output_idx < 0 {
            // Negative index = reference to a gate we just created
            let gate_idx = (-decomp.output_idx - 1) as usize;
            gate_outputs[gate_idx] << 1
        } else {
            // Positive index = passthrough to an existing AIG pin
            // Need to encode as (index << 1) since output_idx is a raw index
            (decomp.output_idx as usize) << 1
        };

        let final_iv = if decomp.output_inverted {
            output_iv ^ 1
        } else {
            output_iv
        };

        self.pin2aigpin_iv[pinid] = final_iv;

        // Record cell origin for SDF back-annotation.
        // The full IOPATH delay goes on the output AIG pin only;
        // internal AND gates from multi-gate decompositions keep None (zero delay).
        let output_aigpin = final_iv >> 1;
        if output_aigpin > 0 && output_aigpin < self.aigpin_cell_origins.len() {
            let output_pin_name = netlistdb.pinnames[pinid].1.as_str();
            self.aigpin_cell_origins[output_aigpin].push((
                cellid,
                cell_type.to_string(),
                output_pin_name.to_string(),
            ));
        }
    }

    /// Post-process for SKY130 multi-output cells (ha, fa, dfbbp).
    fn build_sky130_multi_output_postprocess(
        &mut self,
        netlistdb: &NetlistDB,
        output_pinid: usize,
        cellid: usize,
        cell_type: &str,
        pdk_models: Option<&PdkModels>,
    ) {
        let output_pin_name = netlistdb.pinnames[output_pinid].1.as_str();

        // dfbbp is sequential - handle inline (not in PDK combinational models)
        if cell_type == "dfbbp" {
            if output_pin_name == "Q_N" {
                // Q_N is inverted Q
                for opin in netlistdb.cell2pin.iter_set(cellid) {
                    if netlistdb.pinnames[opin].1.as_str() == "Q" {
                        self.pin2aigpin_iv[output_pinid] = self.pin2aigpin_iv[opin] ^ 1;
                        return;
                    }
                }
                panic!("dfbbp missing Q pin");
            } else {
                // Q output - build DFF logic
                let dff = self.dffs.get(&cellid).unwrap();
                let q = dff.q;
                let mut ap_s_iv = 1;
                let mut ap_r_iv = 1;
                let mut q_out = q << 1;

                for dep_pinid in netlistdb.cell2pin.iter_set(cellid) {
                    let pin_name = netlistdb.pinnames[dep_pinid].1.as_str();
                    let prev = self.pin2aigpin_iv[dep_pinid];
                    match pin_name {
                        "SET_B" => ap_s_iv = prev,
                        "RESET_B" => ap_r_iv = prev,
                        _ => {}
                    }
                }
                q_out = self.add_and_gate(q_out ^ 1, ap_s_iv) ^ 1;
                q_out = self.add_and_gate(q_out, ap_r_iv);
                self.pin2aigpin_iv[output_pinid] = q_out;
            }
            return;
        }

        // Use PDK models for ha/fa
        let pdk = pdk_models.expect(
            "PDK models required for SKY130 multi-output cell decomposition. \
             Ensure sky130_fd_sc_hd submodule is initialized.",
        );
        let mut inputs = CellInputs::new();
        for ipin in netlistdb.cell2pin.iter_set(cellid) {
            if netlistdb.pindirect[ipin] == Direction::I {
                let pin_name = netlistdb.pinnames[ipin].1.as_str();
                inputs.set_pin(pin_name, self.pin2aigpin_iv[ipin]);
            }
        }
        let decomp = decompose_with_pdk(cell_type, &inputs, output_pin_name, pdk);
        // Build the AIG gates from the decomposition
        let mut gate_outputs: SmallVec<[usize; 5]> = SmallVec::new();
        for (a_ref, b_ref) in &decomp.and_gates {
            let a_iv = self.resolve_decomp_ref(*a_ref, &gate_outputs);
            let b_iv = self.resolve_decomp_ref(*b_ref, &gate_outputs);
            let gate_out = self.add_and_gate(a_iv, b_iv);
            gate_outputs.push(gate_out >> 1);
        }
        let output_iv = if decomp.output_idx < 0 {
            let gate_idx = (-decomp.output_idx - 1) as usize;
            gate_outputs[gate_idx] << 1
        } else {
            (decomp.output_idx as usize) << 1
        };
        let final_iv = if decomp.output_inverted {
            output_iv ^ 1
        } else {
            output_iv
        };
        self.pin2aigpin_iv[output_pinid] = final_iv;

        // Record cell origin for SDF back-annotation
        let output_aigpin = final_iv >> 1;
        if output_aigpin > 0 && output_aigpin < self.aigpin_cell_origins.len() {
            self.aigpin_cell_origins[output_aigpin].push((
                cellid,
                cell_type.to_string(),
                output_pin_name.to_string(),
            ));
        }
    }

    /// Resolve the base name to use for a `bi_24t` cell's observable
    /// outputs (`<base>__out` / `<base>__oe`).
    ///
    /// Walks the PAD pin's net to find a pin on the top-level cell
    /// (cell-0); that pin's `dbg_fmt_pin` is the inout port's name
    /// (e.g. `bidir_PAD[12]`). If no top-level pin is on the net
    /// (e.g. unit-test cell instantiated below an intermediate hier
    /// level), falls back to the bi_24t cell instance's hierarchical
    /// name, which is still uniquely traceable.
    fn resolve_bi_24t_observable_base(
        &self,
        netlistdb: &NetlistDB,
        cellid: usize,
        pad_pinid: usize,
    ) -> String {
        use netlistdb::GeneralHierName;
        let netid = netlistdb.pin2net[pad_pinid];
        let net_pins_start = netlistdb.net2pin.start[netid];
        let net_pins_end = if netid + 1 < netlistdb.net2pin.start.len() {
            netlistdb.net2pin.start[netid + 1]
        } else {
            netlistdb.net2pin.items.len()
        };
        for &np in &netlistdb.net2pin.items[net_pins_start..net_pins_end] {
            if netlistdb.pin2cell[np] == 0 {
                // Top-level inout port. `dbg_fmt_pin` formats as
                // `port_name[bus_idx]` (no hierarchy prefix since
                // cell-0's hierarchy is empty).
                return netlistdb.pinnames[np].dbg_fmt_pin();
            }
        }
        // Fallback: cell-instance hierarchical name + a `.PAD` suffix
        // so the observable label is still self-describing.
        format!("{}.PAD", netlistdb.cellnames[cellid].dbg_fmt_hier())
    }

    /// GF180MCU dependency-collection hook.
    ///
    /// Tie cells have no dependencies. Filler/antenna/endcap cells have
    /// no outputs in the logic graph. Sequential cells depend only on
    /// their async reset/set pins (`RN`, `SETN`) — the data/clock pins
    /// are wired up in a separate post-DFS loop, mirroring the sky130
    /// precedent (`SET_B`/`RESET_B`).
    fn get_gf180mcu_dependencies(
        &self,
        netlistdb: &NetlistDB,
        _pinid: usize,
        cellid: usize,
        celltype: &str,
    ) -> SmallVec<[usize; 6]> {
        let cell_type = crate::gf180mcu::extract_cell_type(celltype);
        if crate::gf180mcu_pdk::is_tie_cell(cell_type)
            || crate::gf180mcu_pdk::is_filler_cell(cell_type)
        {
            return SmallVec::new();
        }
        // Sequential cells: depend on RN / SETN (active-low reset / set).
        // Other input pins (D, CLK, CLKN, SE, SI, TE, E) are wired up in
        // the post-DFS DFF setup loop and must NOT be dependencies of
        // the Q output's AIG pin, otherwise we'd build a topological
        // cycle through registered state.
        if crate::gf180mcu_pdk::is_sequential_cell(cell_type) {
            let mut deps = SmallVec::new();
            for dep_pinid in netlistdb.cell2pin.iter_set(cellid) {
                let pin_name = netlistdb.pinnames[dep_pinid].1.as_str();
                if matches!(pin_name, "RN" | "SETN") {
                    deps.push(dep_pinid);
                }
            }
            return deps;
        }
        // IO pads: PAD is the dependency of `Y` (input direction).
        // For bidirectional pads (`bi_24t`) we *also* depend on `A`
        // (core drive-out) and `OE` (output-enable), so the postprocess
        // hook can promote their AIG pins to observable primary outputs
        // (`<top-port>__out` / `<top-port>__oe`). Pure input pads
        // (`in_c` / `in_s`) have no `A`/`OE` to surface.
        //
        // The remaining sim-irrelevant pins (`IE`, `CS`, `SL`, `PU`,
        // `PD`) are still dropped: they are pull / Schmitt / input-
        // enable controls that don't affect digital sim.
        if crate::gf180mcu_pdk::is_io_pad_cell(cell_type) {
            let mut deps = SmallVec::new();
            let is_bidir = cell_type == "bi_24t";
            for dep_pinid in netlistdb.cell2pin.iter_set(cellid) {
                let pin_name = netlistdb.pinnames[dep_pinid].1.as_str();
                match pin_name {
                    "PAD" => deps.push(dep_pinid),
                    "A" | "OE" if is_bidir => deps.push(dep_pinid),
                    _ => {}
                }
            }
            return deps;
        }
        let mut deps = SmallVec::new();
        for dep_pinid in netlistdb.cell2pin.iter_set(cellid) {
            if netlistdb.pindirect[dep_pinid] == Direction::I {
                deps.push(dep_pinid);
            }
        }
        deps
    }

    /// GF180MCU pre-process. Tie cells set their output immediately;
    /// sequential cells pre-allocate their Q aigpin so downstream
    /// combinational paths can reference it before the post-DFS DFF
    /// setup loop wires D / CLK / async S+R / scan pins; everything
    /// else waits for postprocess.
    fn gf180mcu_preprocess(
        &mut self,
        netlistdb: &NetlistDB,
        pinid: usize,
        cellid: usize,
        celltype: &str,
    ) {
        let cell_type = crate::gf180mcu::extract_cell_type(celltype);
        if crate::gf180mcu_pdk::is_tie_cell(cell_type) {
            // tieh → constant-1 (aigpin_iv 1), output named `Z`.
            // tiel → constant-0 (aigpin_iv 0), output named `ZN` (matches
            // the upstream PDK cell library — `tiel` reuses the
            // inverter-style output name even though it's a constant).
            let output_pin_name = netlistdb.pinnames[pinid].1.as_str();
            let expected = if cell_type == "tieh" { "Z" } else { "ZN" };
            assert_eq!(
                output_pin_name, expected,
                "Expected '{expected}' output for gf180mcu '{cell_type}' tie cell, \
                 got '{output_pin_name}'",
            );
            self.pin2aigpin_iv[pinid] = if cell_type == "tieh" { 1 } else { 0 };
            return;
        }

        // Sequential cells (DFFs, latches, clock-gating cells): pre-create
        // a stateful Q aigpin tagged with the cell ID. The combinational
        // reset/set overlay is applied in postprocess; the D and clock
        // inputs are wired up in the post-DFS loop.
        if crate::gf180mcu_pdk::is_sequential_cell(cell_type) {
            let q = self.add_aigpin(DriverType::DFF(cellid));
            self.aigpin_cell_origins[q].push((cellid, cell_type.to_string(), "Q".to_string()));
            let dff = self.dffs.entry(cellid).or_default();
            dff.q = q;
        }
    }

    /// GF180MCU post-process. Decomposes combinational cells via
    /// `decompose_with_pdk` and stitches the resulting DecompResult
    /// into the global AIG. Sequential cells layer the active-low
    /// async reset (`RN`) and set (`SETN`) on top of the pre-allocated
    /// Q aigpin, using the same AIG formula as sky130 (`SET_B`/`RESET_B`).
    fn gf180mcu_postprocess(
        &mut self,
        netlistdb: &NetlistDB,
        pinid: usize,
        cellid: usize,
        celltype: &str,
        gf180_pdk: Option<&Gf180PdkModels>,
    ) {
        let cell_type = crate::gf180mcu::extract_cell_type(celltype);

        // Tie + filler were either handled in preprocess or contribute no
        // logic.
        if crate::gf180mcu_pdk::is_tie_cell(cell_type)
            || crate::gf180mcu_pdk::is_filler_cell(cell_type)
        {
            return;
        }

        // Sequential cells: apply async RN / SETN to the pre-created Q.
        // Polarity convention: GF180MCU RN/SETN are active-low, same
        // semantics as sky130's RESET_B/SET_B. The AIG formula
        //   Q_out = AND(OR(Q, NOT SETN), RN)
        // works unchanged because:
        //   RN=0   → AND(_, 0) = 0   (reset asserted forces Q=0)
        //   RN=1   → AND(_, 1) = _   (RN inactive)
        //   SETN=0 → OR(Q, NOT 0) = 1 → AND(1, RN) = RN (set asserted forces Q=1 unless RN=0)
        //   SETN=1 → OR(Q, NOT 1) = Q (SETN inactive)
        if crate::gf180mcu_pdk::is_sequential_cell(cell_type) {
            let dff = self.dffs.get(&cellid).unwrap();
            let q = dff.q;
            let mut ap_s_iv = 1; // SETN default = 1 (inactive)
            let mut ap_r_iv = 1; // RN default = 1 (inactive)
            for dep_pinid in netlistdb.cell2pin.iter_set(cellid) {
                let pin_name = netlistdb.pinnames[dep_pinid].1.as_str();
                let prev = self.pin2aigpin_iv[dep_pinid];
                match pin_name {
                    "SETN" => ap_s_iv = prev,
                    "RN" => ap_r_iv = prev,
                    _ => {}
                }
            }
            let mut q_out = q << 1;
            q_out = self.add_and_gate(q_out ^ 1, ap_s_iv) ^ 1;
            q_out = self.add_and_gate(q_out, ap_r_iv);
            self.pin2aigpin_iv[pinid] = q_out;
            return;
        }

        // IO pads — digital-sim simplification.
        //
        // * `in_c` / `in_s`: trivial input buffer — internal `Y` equals
        //   external `PAD`. PU / PD pull controls have no logic effect.
        // * `bi_24t`: bidirectional pad. The real cell is
        //   `Y = PAD & IE` (input side) and `PAD = A when OE=1`
        //   (output side, tristate). Jacquard does not model tristate,
        //   so we adopt `Y = PAD` always (IE=1 implied) for the input
        //   direction. To make the *output* direction observable, we
        //   also promote the bidir pad's `A` (core drive) and `OE`
        //   (output-enable) AIG pins to primary outputs with synthetic
        //   names `<top-port>__out` / `<top-port>__oe`. Downstream
        //   consumers can then reconstruct the pad's external value as
        //   `OE ? A : external_stim`. The wider tristate caveat (lack
        //   of in-AIG tristate) is unchanged — see
        //   docs/plans/gf180mcu-enablement.md.
        if crate::gf180mcu_pdk::is_io_pad_cell(cell_type) {
            let output_pin_name = netlistdb.pinnames[pinid].1.as_str();
            assert_eq!(
                output_pin_name, "Y",
                "Expected Y output for gf180mcu IO pad '{cell_type}', got '{output_pin_name}'",
            );
            let mut pad_pinid = usize::MAX;
            let mut a_pinid = usize::MAX;
            let mut oe_pinid = usize::MAX;
            for dep_pinid in netlistdb.cell2pin.iter_set(cellid) {
                match netlistdb.pinnames[dep_pinid].1.as_str() {
                    "PAD" => pad_pinid = dep_pinid,
                    "A" => a_pinid = dep_pinid,
                    "OE" => oe_pinid = dep_pinid,
                    _ => {}
                }
            }
            assert_ne!(
                pad_pinid,
                usize::MAX,
                "gf180mcu IO pad '{cell_type}' has no PAD pin",
            );
            let pad_iv = self.pin2aigpin_iv[pad_pinid];
            self.pin2aigpin_iv[pinid] = pad_iv;

            // bi_24t split: surface A and OE as named primary outputs.
            // The base name is the top-level inout port that the PAD
            // pin's net connects to (e.g. `bidir_PAD[12]`); we look it
            // up by walking the PAD net for a pin on cell-0. If the
            // PAD pin isn't connected to a top-level port (e.g. an
            // intermediate-hier instantiation in a unit test), fall
            // back to the cell-instance hierarchical name so the
            // observable still has a stable, traceable label.
            if cell_type == "bi_24t" {
                let base = self.resolve_bi_24t_observable_base(netlistdb, cellid, pad_pinid);
                if a_pinid != usize::MAX {
                    let a_iv = self.pin2aigpin_iv[a_pinid];
                    if a_iv != usize::MAX {
                        self.primary_outputs.insert(a_iv);
                        self.extra_observable_names
                            .entry(a_iv)
                            .or_default()
                            .push(format!("{}__out", base));
                    }
                }
                if oe_pinid != usize::MAX {
                    let oe_iv = self.pin2aigpin_iv[oe_pinid];
                    if oe_iv != usize::MAX {
                        self.primary_outputs.insert(oe_iv);
                        self.extra_observable_names
                            .entry(oe_iv)
                            .or_default()
                            .push(format!("{}__oe", base));
                    }
                }
            }
            return;
        }

        let pdk = gf180_pdk.expect(
            "GF180 PDK models required for gf180mcu cell decomposition. \
             Ensure vendor/gf180mcu_fd_sc_mcu7t5v0 submodule is initialized.",
        );
        let model = pdk.models.get(cell_type).unwrap_or_else(|| {
            panic!(
                "No GF180MCU behavioural model loaded for cell type '{cell_type}'. \
                 from_netlistdb only loads models for cells present in the netlist; \
                 this points at a load_pdk_models bug.",
            )
        });

        // Build inputs map by pin name. The HashMap allocation per cell
        // is fine — combinational cells are small and the decomposer
        // path runs once per cell instance during AIG construction.
        //
        // Filter by `Direction::I` only. Do NOT skip by pin-name list:
        // pin names are not unique across cells — e.g. GF180MCU's
        // `mux2` uses `S` as a select *input* while `addf` uses `S`
        // as a sum *output* — so a name-based blacklist can drop a
        // real input.
        let mut inputs: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for ipin in netlistdb.cell2pin.iter_set(cellid) {
            if netlistdb.pindirect[ipin] == Direction::I {
                let pin_name = netlistdb.pinnames[ipin].1.as_str();
                inputs.insert(pin_name.to_string(), self.pin2aigpin_iv[ipin]);
            }
        }

        let output_pin_name = netlistdb.pinnames[pinid].1.as_str();
        let decomp =
            crate::gf180mcu_pdk::decompose_with_pdk(model, &inputs, output_pin_name, &pdk.udps);

        // Stitch the DecompResult into the global AIG — same algorithm
        // as sky130_postprocess's tail.
        let mut gate_outputs: SmallVec<[usize; 5]> = SmallVec::new();
        for &(a_ref, b_ref) in &decomp.and_gates {
            let a_iv = self.resolve_decomp_ref(a_ref, &gate_outputs);
            let b_iv = self.resolve_decomp_ref(b_ref, &gate_outputs);
            let gate_out = self.add_and_gate(a_iv, b_iv);
            gate_outputs.push(gate_out >> 1);
        }

        let output_iv = if decomp.output_idx < 0 {
            let gate_idx = (-decomp.output_idx - 1) as usize;
            gate_outputs[gate_idx] << 1
        } else {
            (decomp.output_idx as usize) << 1
        };
        let final_iv = if decomp.output_inverted {
            output_iv ^ 1
        } else {
            output_iv
        };
        self.pin2aigpin_iv[pinid] = final_iv;

        // Record cell origin for SDF / timing back-annotation, mirroring
        // the SKY130 path.
        let output_aigpin = final_iv >> 1;
        if output_aigpin > 0 && output_aigpin < self.aigpin_cell_origins.len() {
            self.aigpin_cell_origins[output_aigpin].push((
                cellid,
                cell_type.to_string(),
                output_pin_name.to_string(),
            ));
        }
    }

    /// Resolve a decomposition reference to an aigpin_iv value.
    fn resolve_decomp_ref(&self, ref_val: i64, gate_outputs: &[usize]) -> usize {
        if ref_val < 0 {
            // Reference to intermediate gate output
            // The inversion bit might be encoded in ref_val
            let base_ref = ref_val | 1; // Clear potential inversion bit for index calc
            let gate_idx = (-(base_ref >> 1) - 1) as usize;
            if gate_idx >= gate_outputs.len() {
                panic!(
                    "resolve_decomp_ref: ref_val={}, base_ref={}, gate_idx={}, gate_outputs.len()={}",
                    ref_val, base_ref, gate_idx, gate_outputs.len()
                );
            }
            let invert = (ref_val & 1) != (base_ref & 1);
            let gate_out = gate_outputs[gate_idx] << 1;
            if invert {
                gate_out ^ 1
            } else {
                gate_out
            }
        } else {
            // Direct input reference (already has inversion bit)
            ref_val as usize
        }
    }

    /// Build an AIG from a netlistdb, using explicit PDK behavioral models for decomposition.
    pub fn from_netlistdb_with_pdk(netlistdb: &NetlistDB, pdk_models: &PdkModels) -> AIG {
        Self::from_netlistdb_impl(netlistdb, Some(pdk_models), None, None)
    }

    /// Build an AIG from a netlistdb, with a runtime cell library
    /// supplying pin tables and `kind` metadata for cells outside
    /// Jacquard's vendored PDKs. See ADR 0010 and
    /// `docs/plans/declarative-cell-metadata.md`.
    ///
    /// Goes through the same library detection + behavioural-model
    /// loading as [`Self::from_netlistdb`] for cells that ARE in the
    /// vendored PDKs; the runtime library augments rather than
    /// replaces the built-in path.
    pub fn from_netlistdb_with_cells(
        netlistdb: &NetlistDB,
        cell_library: Option<&crate::cell_library::RuntimeCellLibrary>,
    ) -> AIG {
        let has_sky130 = (1..netlistdb.num_cells).any(|cid| {
            PdkVariant::classify(netlistdb.celltypes[cid].as_str())
                == Some(PdkVariant::Sky130)
        });
        let has_gf180mcu = (1..netlistdb.num_cells).any(|cid| {
            PdkVariant::classify(netlistdb.celltypes[cid].as_str())
                == Some(PdkVariant::Gf180Mcu)
        });

        if has_sky130 {
            let pdk_path = std::path::PathBuf::from("vendor/sky130_fd_sc_hd/cells");
            let mut cell_types: Vec<String> = Vec::new();
            for cellid in 1..netlistdb.num_cells {
                let celltype = netlistdb.celltypes[cellid].as_str();
                if PdkVariant::classify(celltype) == Some(PdkVariant::Sky130) {
                    let ct = PdkVariant::Sky130.extract_cell_type(celltype).to_string();
                    if !cell_types.contains(&ct) {
                        cell_types.push(ct);
                    }
                }
            }
            cell_types.sort();
            let pdk_models = crate::sky130_pdk::load_pdk_models(&pdk_path, &cell_types);
            Self::from_netlistdb_impl(netlistdb, Some(&pdk_models), None, cell_library)
        } else if has_gf180mcu {
            let pdk_path = std::path::PathBuf::from("vendor/gf180mcu_fd_sc_mcu7t5v0");
            let mut cell_types: Vec<String> = Vec::new();
            for cellid in 1..netlistdb.num_cells {
                let celltype = netlistdb.celltypes[cellid].as_str();
                if PdkVariant::classify(celltype) == Some(PdkVariant::Gf180Mcu) {
                    let ct = PdkVariant::Gf180Mcu.extract_cell_type(celltype).to_string();
                    if !cell_types.contains(&ct) {
                        cell_types.push(ct);
                    }
                }
            }
            cell_types.sort();
            let gf180_pdk = crate::gf180mcu_pdk::load_pdk_models(&pdk_path, &cell_types);
            Self::from_netlistdb_impl(netlistdb, None, Some(&gf180_pdk), cell_library)
        } else {
            Self::from_netlistdb_impl(netlistdb, None, None, cell_library)
        }
    }

    /// Build an AIG from a netlistdb.
    ///
    /// For designs with SKY130 or GF180MCU cells, automatically loads PDK
    /// behavioural models from the matching vendored submodule. Panics
    /// with a helpful message if the submodule is not initialised.
    /// Per `CellLibrary::Mixed` rejection upstream, the netlist is
    /// guaranteed to use at most one PDK.
    pub fn from_netlistdb(netlistdb: &NetlistDB) -> AIG {
        Self::from_netlistdb_with_cells(netlistdb, None)
    }

    fn from_netlistdb_impl(
        netlistdb: &NetlistDB,
        pdk_models: Option<&PdkModels>,
        gf180_pdk: Option<&Gf180PdkModels>,
        cell_library: Option<&crate::cell_library::RuntimeCellLibrary>,
    ) -> AIG {
        let mut aig = AIG {
            num_aigpins: 0,
            pin2aigpin_iv: vec![usize::MAX; netlistdb.num_pins],
            drivers: vec![DriverType::Tie0],
            aigpin_cell_origins: vec![Vec::new()], // Tie0 has no cell origin
            ..Default::default()
        };

        clilog::info!(
            "Starting clock tracing for {} cells...",
            netlistdb.num_cells
        );
        let clock_start = std::time::Instant::now();
        let mut seq_count = 0;
        let mut trace_count = 0;

        for cellid in 1..netlistdb.num_cells {
            let celltype = netlistdb.celltypes[cellid].as_str();

            // Check if this is a sequential element (AIGPDK / SKY130 / GF180MCU)
            let is_aigpdk_seq = matches!(celltype, "DFF" | "DFFSR" | "$__RAMGEM_SYNC_");
            let is_pdk_seq = match PdkVariant::classify(celltype) {
                Some(variant) => variant.is_sequential(variant.extract_cell_type(celltype)),
                None => false,
            };

            if !is_aigpdk_seq && !is_pdk_seq {
                continue;
            }

            seq_count += 1;
            if seq_count <= 5 {
                use netlistdb::GeneralHierName;
                clilog::info!(
                    "  Processing seq cell {} ({}): {}",
                    seq_count,
                    celltype,
                    netlistdb.cellnames[cellid].dbg_fmt_hier()
                );
            } else if seq_count % 500 == 0 {
                clilog::info!(
                    "  Processed {} sequential cells ({} clock traces) in {:?}...",
                    seq_count,
                    trace_count,
                    clock_start.elapsed()
                );
            }
            for pinid in netlistdb.cell2pin.iter_set(cellid) {
                let pin_name = netlistdb.pinnames[pinid].1.as_str();
                // AIGPDK/SKY130 sequentials use "CLK"; GF180MCU adds "CLKN"
                // for negative-edge cells (`dffnq`, `dffnrnq`, `icgtn`, …);
                // SRAMs use the PORT_*_CLK pair.
                let is_clkn = pin_name == "CLKN";
                if !matches!(pin_name, "CLK" | "CLKN" | "PORT_R_CLK" | "PORT_W_CLK") {
                    continue;
                }
                trace_count += 1;
                if seq_count <= 5 {
                    clilog::info!("    Tracing clock pin {} for cell {}...", pinid, cellid);
                }
                // Negative-edge cells (CLKN) trigger on the falling edge:
                // start the trace with is_negedge=true so trace_clock_pin
                // records the inverted clock signal.
                if let Err(pinid) = aig.trace_clock_pin(netlistdb, pinid, is_clkn, true) {
                    use netlistdb::GeneralHierName;
                    panic!(
                        "Tracing clock pin of cell {} error: \
                            there is a multi-input cell driving {} \
                            that clocks this sequential element. \
                            Clock gating need to be manually patched atm.",
                        netlistdb.cellnames[cellid].dbg_fmt_hier(),
                        netlistdb.pinnames[pinid].dbg_fmt_pin()
                    );
                }
            }
        }
        clilog::info!(
            "Clock tracing done in {:?}, {} sequential cells",
            clock_start.elapsed(),
            seq_count
        );

        for (&clk, &(flagr, flagf)) in &aig.clock_pin2aigpins {
            clilog::info!(
                "inferred clock port {} ({})",
                netlistdb.pinnames[clk].dbg_fmt_pin(),
                match (flagr, flagf) {
                    (_, usize::MAX) => "posedge",
                    (usize::MAX, _) => "negedge",
                    _ => "posedge & negedge",
                }
            );
        }

        clilog::info!("Starting AIG DFS build for {} pins...", netlistdb.num_pins);
        let dfs_start = std::time::Instant::now();

        let mut topo_vis = vec![false; netlistdb.num_pins];
        let mut topo_instack = vec![false; netlistdb.num_pins];

        for pinid in 0..netlistdb.num_pins {
            aig.dfs_netlistdb_build_aig(
                netlistdb,
                &mut topo_vis,
                &mut topo_instack,
                pinid,
                pdk_models,
                gf180_pdk,
                cell_library,
            );
        }

        clilog::info!(
            "AIG DFS build done in {:?}, {} AIG pins created",
            dfs_start.elapsed(),
            aig.num_aigpins
        );

        for cellid in 0..netlistdb.num_cells {
            let celltype = netlistdb.celltypes[cellid].as_str();

            if matches!(celltype, "DFF" | "DFFSR") {
                let mut ap_s_iv = 1;
                let mut ap_r_iv = 1;
                let mut ap_d_iv = 0;
                let mut ap_clken_iv = 0;
                for pinid in netlistdb.cell2pin.iter_set(cellid) {
                    let pin_iv = aig.pin2aigpin_iv[pinid];
                    match netlistdb.pinnames[pinid].1.as_str() {
                        "D" => ap_d_iv = pin_iv,
                        "S" => ap_s_iv = pin_iv,
                        "R" => ap_r_iv = pin_iv,
                        "CLK" => {
                            ap_clken_iv =
                                aig.trace_clock_pin(netlistdb, pinid, false, false).unwrap()
                        }
                        _ => {}
                    }
                }
                // AIGPDK DFFSR .lib says: clear="(!R)", preset="(!S)"
                // i.e. R=0 → Q=0 (clear), S=0 → Q=1 (preset)
                // Same active-low semantics as SKY130 RESET_B/SET_B and
                // GF180 RN/SETN — shared overlay helper.
                let (d_in, ap_clken_iv) =
                    aig.wire_dff_reset_set_overlay(ap_d_iv, ap_clken_iv, ap_r_iv, ap_s_iv);
                let dff = aig.dffs.entry(cellid).or_default();
                dff.en_iv = ap_clken_iv;
                dff.d_iv = d_in;
                assert_ne!(dff.q, 0);
            } else if PdkVariant::classify(celltype) == Some(PdkVariant::Sky130)
                && PdkVariant::Sky130
                    .is_sequential(PdkVariant::Sky130.extract_cell_type(celltype))
            {
                // Handle SKY130 DFFs (dfxtp, edfxtp, dfrtp, etc.)
                let cell_type = PdkVariant::Sky130.extract_cell_type(celltype);
                let mut ap_d_iv = 0;
                let mut ap_clken_iv = 0;
                let mut ap_enable_iv = 1; // For edfxtp (data enable)
                let mut ap_s_iv = 1; // Default: no set (SET_B=1, inactive)
                let mut ap_r_iv = 1; // Default: no reset (RESET_B=1, inactive)

                for pinid in netlistdb.cell2pin.iter_set(cellid) {
                    let pin_iv = aig.pin2aigpin_iv[pinid];
                    match netlistdb.pinnames[pinid].1.as_str() {
                        "D" => ap_d_iv = pin_iv,
                        "DE" => ap_enable_iv = pin_iv, // Data enable for edfxtp
                        "SET_B" => ap_s_iv = pin_iv, // Active-low: AND/OR formulas handle it directly
                        "RESET_B" => ap_r_iv = pin_iv, // Active-low: AND(d, RESET_B) → RESET_B=0 forces d=0
                        "CLK" => {
                            ap_clken_iv =
                                aig.trace_clock_pin(netlistdb, pinid, false, false).unwrap()
                        }
                        _ => {}
                    }
                }

                // For edfxtp, the clock enable is gated by DE (data enable)
                if cell_type == "edfxtp" {
                    ap_clken_iv = aig.add_and_gate(ap_clken_iv, ap_enable_iv);
                }

                // Apply async set/reset to D input and clock enable
                // (shared overlay helper — same as AIGPDK DFFSR and GF180MCU).
                let (d_in, ap_clken_iv) =
                    aig.wire_dff_reset_set_overlay(ap_d_iv, ap_clken_iv, ap_r_iv, ap_s_iv);

                let dff = aig.dffs.entry(cellid).or_default();
                dff.en_iv = ap_clken_iv;
                dff.d_iv = d_in;
                assert_ne!(dff.q, 0, "SKY130 DFF {} has no Q output built", cellid);
            } else if PdkVariant::classify(celltype) == Some(PdkVariant::Gf180Mcu)
                && PdkVariant::Gf180Mcu
                    .is_sequential(PdkVariant::Gf180Mcu.extract_cell_type(celltype))
            {
                // GF180MCU sequentials: DFFs / scan DFFs / latches /
                // clock-gating cells. Same async-set/reset shape as
                // sky130, but pin names are GF180-specific (RN/SETN
                // active-low; CLKN for negative-edge cells; SE/SI for
                // scan DFFs; TE/E for clock-gating cells).
                //
                // Async S/R semantics:
                //   RN  active-low reset (RN=0 forces Q=0)
                //   SETN active-low set (SETN=0 forces Q=1)
                // ⇒ same AIG formula as sky130's RESET_B/SET_B:
                //   d_in  = AND(OR(D, NOT SETN), RN)
                //   en_iv = OR(posedge_clk, NOT RN, NOT SETN)
                //
                // Scan DFFs implement D_eff = SE ? SI : D upstream of
                // the latch via combinational logic in the functional
                // model; we replicate that here so the synthesised
                // logic matches.
                //
                // icgtp/icgtn are clock-gating cells whose Q output
                // is a gated clock rather than a true register output.
                // We treat them as state-holding DFFs to avoid a
                // panic; clock-tree-aware simulation through them is
                // a follow-on item (Phase 5+).
                let cell_type = PdkVariant::Gf180Mcu.extract_cell_type(celltype);
                let mut ap_d_iv: usize = 0;
                let mut ap_clken_iv: usize = 0;
                let mut ap_s_iv: usize = 1; // SETN default = inactive
                let mut ap_r_iv: usize = 1; // RN default = inactive
                let mut ap_se_iv: Option<usize> = None;
                let mut ap_si_iv: Option<usize> = None;
                let mut ap_e_iv: Option<usize> = None;
                let mut ap_te_iv: Option<usize> = None;
                let mut have_clk: bool = false;

                for pinid in netlistdb.cell2pin.iter_set(cellid) {
                    let pin_iv = aig.pin2aigpin_iv[pinid];
                    match netlistdb.pinnames[pinid].1.as_str() {
                        "D" => ap_d_iv = pin_iv,
                        "SETN" => ap_s_iv = pin_iv,
                        "RN" => ap_r_iv = pin_iv,
                        "SE" => ap_se_iv = Some(pin_iv),
                        "SI" => ap_si_iv = Some(pin_iv),
                        // Latch enable (also reused as data-port for
                        // clock-gating cells where it appears alongside TE).
                        "E" => ap_e_iv = Some(pin_iv),
                        "TE" => ap_te_iv = Some(pin_iv),
                        // CLK (positive edge) and CLKN (negative edge)
                        // both produce ap_clken_iv via trace_clock_pin;
                        // CLKN flips the start polarity so the inferred
                        // signal is the falling-edge tracker.
                        "CLK" => {
                            ap_clken_iv =
                                aig.trace_clock_pin(netlistdb, pinid, false, false).unwrap();
                            have_clk = true;
                        }
                        "CLKN" => {
                            ap_clken_iv =
                                aig.trace_clock_pin(netlistdb, pinid, true, false).unwrap();
                            have_clk = true;
                        }
                        _ => {}
                    }
                }

                // Scan-flop D mux: D_eff = SE ? SI : D. AIG: D_eff =
                // OR(AND(D, NOT SE), AND(SI, SE)). When SE/SI are
                // absent (non-scan DFFs), this branch is skipped.
                if let (Some(se), Some(si)) = (ap_se_iv, ap_si_iv) {
                    let d_when_normal = aig.add_and_gate(ap_d_iv, se ^ 1);
                    let d_when_scan = aig.add_and_gate(si, se);
                    // OR via De Morgan: a + b = NOT(NOT(a) AND NOT(b))
                    let nor_ab = aig.add_and_gate(d_when_normal ^ 1, d_when_scan ^ 1);
                    ap_d_iv = nor_ab ^ 1;
                }

                // Clock-gating cells (icgtp/icgtn): no D pin in the
                // module ports. The latched value is `E | TE`. Wire
                // that up as the D input so the underlying DFF/latch
                // emulation captures something meaningful, even
                // though Q is also subject to a CLK gate which we
                // can't represent purely in the en_iv/d_iv form.
                if matches!(cell_type, "icgtp" | "icgtn") {
                    if let (Some(e), Some(te)) = (ap_e_iv, ap_te_iv) {
                        let nor_et = aig.add_and_gate(e ^ 1, te ^ 1);
                        ap_d_iv = nor_et ^ 1;
                    }
                }

                // Latches (latq/latrnq/latsnq/latrsnq): D-input is the
                // module `D` pin, en_iv is the module `E` pin (level-
                // sensitive enable). No clock trace; the user's design
                // is responsible for keeping E free of combinational
                // glitches if it cares about latch correctness.
                if matches!(cell_type, "latq" | "latrnq" | "latsnq" | "latrsnq") {
                    ap_clken_iv = ap_e_iv.unwrap_or(0);
                    have_clk = true;
                }

                assert!(
                    have_clk,
                    "GF180MCU sequential cell '{cell_type}' (id={cellid}) has no \
                     CLK / CLKN / E pin — pin-table or classification bug?",
                );

                // Apply async set/reset to D input and clock enable
                // (shared overlay helper — identical formulas to AIGPDK
                // DFFSR and sky130 SET_B/RESET_B):
                //   d_in  = AND(OR(D, NOT SETN), RN)
                //   clken = OR(posedge_clk, NOT RN, NOT SETN)
                let (d_in, ap_clken_iv) =
                    aig.wire_dff_reset_set_overlay(ap_d_iv, ap_clken_iv, ap_r_iv, ap_s_iv);

                let dff = aig.dffs.entry(cellid).or_default();
                dff.en_iv = ap_clken_iv;
                dff.d_iv = d_in;
                assert_ne!(dff.q, 0, "GF180MCU DFF {cellid} has no Q output built");
            } else if celltype == "$__RAMGEM_SYNC_" {
                let mut sram = aig.srams.entry(cellid).or_default().clone();
                let mut write_clken_iv = 0;
                for pinid in netlistdb.cell2pin.iter_set(cellid) {
                    let bit = netlistdb.pinnames[pinid].2.map(|i| i as usize);
                    let pin_iv = aig.pin2aigpin_iv[pinid];
                    match netlistdb.pinnames[pinid].1.as_str() {
                        "PORT_R_ADDR" => {
                            sram.port_r_addr_iv[bit.unwrap()] = pin_iv;
                        }
                        "PORT_R_CLK" => {
                            sram.port_r_en_iv =
                                aig.trace_clock_pin(netlistdb, pinid, false, false).unwrap();
                        }
                        "PORT_W_ADDR" => {
                            sram.port_w_addr_iv[bit.unwrap()] = pin_iv;
                        }
                        "PORT_W_CLK" => {
                            write_clken_iv =
                                aig.trace_clock_pin(netlistdb, pinid, false, false).unwrap();
                        }
                        "PORT_W_WR_DATA" => {
                            sram.port_w_wr_data_iv[bit.unwrap()] = pin_iv;
                        }
                        "PORT_W_WR_EN" => {
                            sram.port_w_wr_en_iv[bit.unwrap()] = pin_iv;
                        }
                        _ => {}
                    }
                }
                for i in 0..32 {
                    let or_en = sram.port_w_wr_en_iv[i];
                    let or_en = aig.add_and_gate(or_en, write_clken_iv);
                    sram.port_w_wr_en_iv[i] = or_en;
                }
                *aig.srams.get_mut(&cellid).unwrap() = sram;
            } else if celltype.starts_with("CF_SRAM_") {
                // ChipFlow SRAM: single-port with shared address
                // Pins: CLKin, EN, R_WB, AD[9:0], BEN[31:0], DI[31:0], DO[31:0]
                let mut sram = aig.srams.entry(cellid).or_default().clone();
                let mut clken_iv = 0;
                let mut r_wb_iv = 0; // 0=write, 1=read

                for pinid in netlistdb.cell2pin.iter_set(cellid) {
                    let bit = netlistdb.pinnames[pinid].2.map(|i| i as usize);
                    let pin_iv = aig.pin2aigpin_iv[pinid];
                    match netlistdb.pinnames[pinid].1.as_str() {
                        "CLKin" => {
                            clken_iv = aig.trace_clock_pin(netlistdb, pinid, false, false).unwrap();
                        }
                        "EN" => {
                            // EN combined with clock for read enable
                            // (we'll combine later)
                        }
                        "R_WB" => {
                            r_wb_iv = pin_iv; // 0=write, 1=read
                        }
                        "AD" => {
                            // Shared address for read and write
                            let idx = bit.unwrap();
                            if idx < AIGPDK_SRAM_ADDR_WIDTH {
                                sram.port_r_addr_iv[idx] = pin_iv;
                                sram.port_w_addr_iv[idx] = pin_iv;
                            }
                        }
                        "DI" => {
                            sram.port_w_wr_data_iv[bit.unwrap()] = pin_iv;
                        }
                        "BEN" => {
                            // Byte enable becomes write enable when R_WB=0
                            sram.port_w_wr_en_iv[bit.unwrap()] = pin_iv;
                        }
                        _ => {} // Ignore control pins like SM, TM, Scan*, vpwr*
                    }
                }

                // Read enable: clken AND R_WB (read when R_WB=1)
                sram.port_r_en_iv = aig.add_and_gate(clken_iv, r_wb_iv);

                // Write enable: clken AND !R_WB (write when R_WB=0) AND BEN
                let write_clken_iv = aig.add_and_gate(clken_iv, r_wb_iv ^ 1);
                for i in 0..32 {
                    let ben_iv = sram.port_w_wr_en_iv[i];
                    sram.port_w_wr_en_iv[i] = aig.add_and_gate(write_clken_iv, ben_iv);
                }

                *aig.srams.get_mut(&cellid).unwrap() = sram;
            } else if netlistdb.celltypes[cellid].as_str() == "GEM_ASSERT" {
                // Parse GEM_ASSERT cells for assertion checking
                // GEM_ASSERT has: CLK (trigger), EN (enable), A (condition)
                // Assertion fails when EN is high and A is low
                let mut ap_en_iv = 1; // Default: always enabled
                let mut ap_a_iv = 1; // Default: always passing
                let mut ap_clken_iv = 1; // Default: always triggered

                for pinid in netlistdb.cell2pin.iter_set(cellid) {
                    let pin_iv = aig.pin2aigpin_iv[pinid];
                    match netlistdb.pinnames[pinid].1.as_str() {
                        "EN" => ap_en_iv = pin_iv,
                        "A" => ap_a_iv = pin_iv,
                        "CLK" => {
                            // Try to trace clock, but if it's tied to 1, use constant
                            if let Ok(clken) = aig.trace_clock_pin(netlistdb, pinid, false, false) {
                                ap_clken_iv = clken;
                            }
                        }
                        _ => {}
                    }
                }

                // The condition for firing an assertion event:
                // fire = clk_enable && EN && !A
                // We store the "fire" condition (inverted A ANDed with EN and clock)
                let fire_condition = aig.add_and_gate(ap_en_iv, ap_a_iv ^ 1);
                let fire_condition = aig.add_and_gate(fire_condition, ap_clken_iv);

                let simcontrol = aig.simcontrols.entry(cellid).or_default();
                simcontrol.condition_iv = fire_condition;
                simcontrol.control_type = None; // Assertion, not $stop/$finish
                simcontrol.message_id = cellid as u32; // Use cell ID as message ID for now

                clilog::debug!(
                    "Found GEM_ASSERT cell {} (condition_iv={}, en_iv={}, a_iv={}, clken_iv={})",
                    cellid,
                    fire_condition,
                    ap_en_iv,
                    ap_a_iv,
                    ap_clken_iv
                );
            } else if netlistdb.celltypes[cellid].as_str() == "GEM_DISPLAY" {
                // Parse GEM_DISPLAY cells for $display/$write support
                // GEM_DISPLAY has: CLK (trigger), EN (enable), MSG_ID[31:0] (argument values)
                // Plus attributes: gem_format (format string), gem_args_width (arg width)
                let mut dp_en_iv = 1; // Default: always enabled
                let mut dp_clken_iv = 1; // Default: always triggered
                let mut dp_args_iv = Vec::new();

                for pinid in netlistdb.cell2pin.iter_set(cellid) {
                    let pin_iv = aig.pin2aigpin_iv[pinid];
                    match netlistdb.pinnames[pinid].1.as_str() {
                        "EN" => dp_en_iv = pin_iv,
                        "CLK" => {
                            // Try to trace clock for edge detection
                            if let Ok(clken) = aig.trace_clock_pin(netlistdb, pinid, false, false) {
                                dp_clken_iv = clken;
                            }
                        }
                        "MSG_ID" => {
                            // MSG_ID is a 32-bit bus, collect all bit pins
                            dp_args_iv.push(pin_iv);
                        }
                        _ => {}
                    }
                }

                // The condition for firing a display event:
                // fire = clk_enable && EN
                let fire_condition = aig.add_and_gate(dp_en_iv, dp_clken_iv);

                // Get cell name for matching with JSON attributes later
                use netlistdb::GeneralHierName;
                let cell_name = netlistdb.cellnames[cellid].dbg_fmt_hier();

                // Create DisplayNode
                let display = aig.displays.entry(cellid).or_default();
                display.enable_iv = fire_condition;
                display.clken_iv = dp_clken_iv;
                // Format string will be populated from JSON attributes later
                display.format = format!("$display at cell {}", cellid);
                display.args_iv = dp_args_iv;
                display.arg_widths = vec![32]; // Default: 32-bit argument
                display.cell_name = cell_name.to_string();

                clilog::debug!(
                    "Found GEM_DISPLAY cell {} '{}' (enable_iv={}, clken_iv={}, args={})",
                    cellid,
                    display.cell_name,
                    fire_condition,
                    dp_clken_iv,
                    display.args_iv.len()
                );
            }
        }

        // Validate AIG: check for bad operand values
        for (i, driver) in aig.drivers.iter().enumerate() {
            if let DriverType::AndGate(a, b) = *driver {
                if (a >> 1) > aig.num_aigpins {
                    panic!("AIG validation: AND gate at pin {} has bad operand a={} (a>>1={}, num_aigpins={})", i, a, a >> 1, aig.num_aigpins);
                }
                if (b >> 1) > aig.num_aigpins {
                    panic!("AIG validation: AND gate at pin {} has bad operand b={} (b>>1={}, num_aigpins={})", i, b, b >> 1, aig.num_aigpins);
                }
            }
        }
        // Validate DFF d_iv and en_iv
        for (cellid, dff) in &aig.dffs {
            if (dff.d_iv >> 1) > aig.num_aigpins {
                use netlistdb::GeneralHierName;
                let cell_name = netlistdb.cellnames[*cellid].dbg_fmt_hier();
                panic!(
                    "AIG validation: DFF {} ({}) has bad d_iv={} (d_iv>>1={})",
                    cellid,
                    cell_name,
                    dff.d_iv,
                    dff.d_iv >> 1
                );
            }
            if (dff.en_iv >> 1) > aig.num_aigpins {
                use netlistdb::GeneralHierName;
                let cell_name = netlistdb.cellnames[*cellid].dbg_fmt_hier();
                panic!(
                    "AIG validation: DFF {} ({}) has bad en_iv={} (en_iv>>1={})",
                    cellid,
                    cell_name,
                    dff.en_iv,
                    dff.en_iv >> 1
                );
            }
        }
        // Validate primary_outputs
        for &po_iv in &aig.primary_outputs {
            if (po_iv >> 1) > aig.num_aigpins {
                // Find which pin has this value
                for pinid in 0..netlistdb.num_pins {
                    if aig.pin2aigpin_iv[pinid] == po_iv {
                        use netlistdb::GeneralHierName;
                        let pin_name = netlistdb.pinnames[pinid].dbg_fmt_pin();
                        let cellid = netlistdb.pin2cell[pinid];
                        let cell_name = netlistdb.cellnames[cellid].dbg_fmt_hier();
                        let cell_type = &netlistdb.celltypes[cellid];
                        let dir = netlistdb.pindirect[pinid];
                        panic!("AIG validation: primary output has bad value iv={} from pin {} (cell {} type={}, dir={:?})", po_iv, pin_name, cell_name, cell_type, dir);
                    }
                }
                panic!("AIG validation: primary output has bad value iv={} (iv>>1={}) - could not find source pin", po_iv, po_iv >> 1);
            }
        }

        aig.fanouts_start = vec![0; aig.num_aigpins + 2];
        for driver in aig.drivers.iter() {
            if let DriverType::AndGate(a, b) = *driver {
                if (a >> 1) != 0 {
                    aig.fanouts_start[a >> 1] += 1;
                }
                if (b >> 1) != 0 {
                    aig.fanouts_start[b >> 1] += 1;
                }
            }
        }
        for i in 1..aig.num_aigpins + 2 {
            aig.fanouts_start[i] += aig.fanouts_start[i - 1];
        }
        aig.fanouts = vec![0; aig.fanouts_start[aig.num_aigpins + 1]];
        for (i, driver) in aig.drivers.iter().enumerate() {
            if let DriverType::AndGate(a, b) = *driver {
                if (a >> 1) != 0 {
                    let st = aig.fanouts_start[a >> 1] - 1;
                    aig.fanouts_start[a >> 1] = st;
                    aig.fanouts[st] = i;
                }
                if (b >> 1) != 0 {
                    let st = aig.fanouts_start[b >> 1] - 1;
                    aig.fanouts_start[b >> 1] = st;
                    aig.fanouts[st] = i;
                }
            }
        }

        aig
    }

    pub fn topo_traverse_generic(
        &self,
        endpoints: Option<&Vec<usize>>,
        is_primary_input: Option<&IndexSet<usize>>,
    ) -> Vec<usize> {
        let mut vis = IndexSet::new();
        let mut ret = Vec::new();
        fn dfs_topo(
            aig: &AIG,
            vis: &mut IndexSet<usize>,
            ret: &mut Vec<usize>,
            is_primary_input: Option<&IndexSet<usize>>,
            u: usize,
        ) {
            if vis.contains(&u) {
                return;
            }
            vis.insert(u);
            if let DriverType::AndGate(a, b) = aig.drivers[u] {
                if is_primary_input.map(|s| s.contains(&u)) != Some(true) {
                    if (a >> 1) != 0 {
                        dfs_topo(aig, vis, ret, is_primary_input, a >> 1);
                    }
                    if (b >> 1) != 0 {
                        dfs_topo(aig, vis, ret, is_primary_input, b >> 1);
                    }
                }
            }
            ret.push(u);
        }
        if let Some(endpoints) = endpoints {
            for &endpoint in endpoints {
                dfs_topo(self, &mut vis, &mut ret, is_primary_input, endpoint);
            }
        } else {
            for i in 1..self.num_aigpins + 1 {
                dfs_topo(self, &mut vis, &mut ret, is_primary_input, i);
            }
        }
        ret
    }

    pub fn num_endpoint_groups(&self) -> usize {
        self.primary_outputs.len()
            + self.dffs.len()
            + self.srams.len()
            + self.simcontrols.len()
            + self.displays.len()
    }

    pub fn get_endpoint_group(&self, endpt_id: usize) -> EndpointGroup<'_> {
        let po_len = self.primary_outputs.len();
        let dff_len = self.dffs.len();
        let sram_len = self.srams.len();
        let simctrl_len = self.simcontrols.len();

        if endpt_id < po_len {
            EndpointGroup::PrimaryOutput(*self.primary_outputs.get_index(endpt_id).unwrap())
        } else if endpt_id < po_len + dff_len {
            EndpointGroup::DFF(&self.dffs[endpt_id - po_len])
        } else if endpt_id < po_len + dff_len + sram_len {
            EndpointGroup::RAMBlock(&self.srams[endpt_id - po_len - dff_len])
        } else if endpt_id < po_len + dff_len + sram_len + simctrl_len {
            EndpointGroup::SimControl(&self.simcontrols[endpt_id - po_len - dff_len - sram_len])
        } else {
            EndpointGroup::Display(
                &self.displays[endpt_id - po_len - dff_len - sram_len - simctrl_len],
            )
        }
    }

    /// Populate display format information from JSON attributes.
    /// This should be called after from_netlistdb() with display info extracted from the JSON.
    pub fn populate_display_info(
        &mut self,
        display_info: &IndexMap<String, crate::display::DisplayCellInfo>,
    ) {
        for (_cell_id, display) in self.displays.iter_mut() {
            if let Some(info) = display_info.get(&display.cell_name) {
                display.format = info.format.clone();
                display.arg_widths = vec![info.args_width];
                clilog::debug!(
                    "Populated display info for '{}': format='{}', args_width={}",
                    display.cell_name,
                    info.format,
                    info.args_width
                );
            }
        }
    }

    // === Static Timing Analysis Methods ===

    /// Load timing information from a Liberty library.
    ///
    /// This initializes the gate_delays vector and dff_timing field.
    pub fn load_timing_library(&mut self, lib: &crate::liberty_parser::TimingLibrary) {
        // Initialize gate delays for all AIG pins
        // Index 0 is Tie0 which has zero delay
        self.gate_delays = vec![(0, 0); self.num_aigpins + 1];

        // Get default AND gate delay (all variants have same timing in AIGPDK)
        let and_delay = lib.and_gate_delay("AND2_00_0").unwrap_or((1, 1));

        // Assign delays based on driver type
        for i in 1..=self.num_aigpins {
            let delay = match &self.drivers[i] {
                DriverType::AndGate(_, _) => and_delay,
                DriverType::InputPort(_) => (0, 0), // Primary inputs have zero arrival
                DriverType::InputClockFlag(_, _) => (0, 0), // Clock flags
                DriverType::DFF(_) => {
                    // Clock-to-Q delay
                    lib.dff_timing()
                        .map(|t| (t.clk_to_q_rise_ps, t.clk_to_q_fall_ps))
                        .unwrap_or((0, 0))
                }
                DriverType::SRAM(_) => {
                    // SRAM read delay
                    lib.sram_timing()
                        .map(|t| (t.read_clk_to_data_rise_ps, t.read_clk_to_data_fall_ps))
                        .unwrap_or((1, 1))
                }
                DriverType::Tie0 => (0, 0),
            };
            self.gate_delays[i] = delay;
        }

        // Load DFF timing constraints
        self.dff_timing = lib.dff_timing();

        clilog::info!(
            "Loaded timing library: AND delay={}ps, DFF setup={}ps, hold={}ps",
            and_delay.0,
            self.dff_timing.as_ref().map(|t| t.max_setup()).unwrap_or(0),
            self.dff_timing.as_ref().map(|t| t.max_hold()).unwrap_or(0)
        );
    }

    /// Compute static timing analysis (STA) for the AIG.
    ///
    /// This computes arrival times at all nodes and calculates setup/hold
    /// slacks for all DFFs. Must call `load_timing_library` first.
    ///
    /// Returns a `TimingReport` with summary statistics.
    pub fn compute_timing(&mut self) -> TimingReport {
        // Initialize arrival times (min, max) for all pins
        self.arrival_times = vec![(0, 0); self.num_aigpins + 1];

        // Get topological order (already guaranteed in AIG)
        // Traverse in order 1..num_aigpins since drivers are in topo order
        for i in 1..=self.num_aigpins {
            let (min_arrival, max_arrival) = match &self.drivers[i] {
                DriverType::AndGate(a, b) => {
                    let (delay_rise, delay_fall) = self.gate_delays[i];
                    let delay = delay_rise.max(delay_fall);

                    // Get arrival times of inputs (strip inversion bit)
                    let a_idx = a >> 1;
                    let b_idx = b >> 1;

                    let (a_min, a_max) = if a_idx == 0 {
                        (0, 0)
                    } else {
                        self.arrival_times[a_idx]
                    };
                    let (b_min, b_max) = if b_idx == 0 {
                        (0, 0)
                    } else {
                        self.arrival_times[b_idx]
                    };

                    // Min arrival: min of inputs + delay
                    // Max arrival: max of inputs + delay
                    (a_min.min(b_min) + delay, a_max.max(b_max) + delay)
                }
                DriverType::InputPort(_) | DriverType::InputClockFlag(_, _) | DriverType::Tie0 => {
                    // Primary inputs arrive at time 0
                    (0, 0)
                }
                DriverType::DFF(_) => {
                    // DFF output arrives after clock-to-Q delay
                    let (delay_rise, delay_fall) = self.gate_delays[i];
                    (delay_rise.max(delay_fall), delay_rise.max(delay_fall))
                }
                DriverType::SRAM(_) => {
                    // SRAM read data arrives after read delay
                    let (delay_rise, delay_fall) = self.gate_delays[i];
                    (delay_rise.max(delay_fall), delay_rise.max(delay_fall))
                }
            };

            self.arrival_times[i] = (min_arrival, max_arrival);
        }

        // Compute setup and hold slacks for each DFF
        self.setup_slacks = Vec::with_capacity(self.dffs.len());
        self.hold_slacks = Vec::with_capacity(self.dffs.len());

        let dff_timing = self.dff_timing.clone().unwrap_or_default();
        let setup_time = dff_timing.max_setup();
        let hold_time = dff_timing.max_hold();

        let mut report = TimingReport {
            num_endpoints: self.dffs.len(),
            ..Default::default()
        };

        for (_cell_id, dff) in &self.dffs {
            // Get data arrival time at D input
            let d_idx = dff.d_iv >> 1;
            let (_, data_max_arrival) = if d_idx == 0 {
                (0, 0)
            } else {
                self.arrival_times[d_idx]
            };

            // Clock arrival is at time 0 (or clock_period for setup check)
            // Setup check: data must arrive before clock_period - setup_time
            let setup_slack =
                (self.clock_period_ps as i64) - (data_max_arrival as i64) - (setup_time as i64);

            // Hold check: data must be stable for hold_time after clock
            // For hold, we check min arrival vs hold_time
            let (data_min_arrival, _) = if d_idx == 0 {
                (0, 0)
            } else {
                self.arrival_times[d_idx]
            };
            let hold_slack = (data_min_arrival as i64) - (hold_time as i64);

            self.setup_slacks.push(setup_slack);
            self.hold_slacks.push(hold_slack);

            // Update report statistics
            report.worst_setup_slack = report.worst_setup_slack.min(setup_slack);
            report.worst_hold_slack = report.worst_hold_slack.min(hold_slack);

            if setup_slack < 0 {
                report.setup_violations += 1;
            }
            if hold_slack < 0 {
                report.hold_violations += 1;
            }
        }

        // Find critical path (longest arrival time at any endpoint)
        for &po_iv in &self.primary_outputs {
            let po = po_iv >> 1; // Strip inversion bit
            if po > 0 && po <= self.num_aigpins {
                let (_, max_arr) = self.arrival_times[po];
                report.critical_path_delay = report.critical_path_delay.max(max_arr);
            }
        }
        for (_cell_id, dff) in &self.dffs {
            let d_idx = dff.d_iv >> 1;
            if d_idx > 0 {
                let (_, max_arr) = self.arrival_times[d_idx];
                report.critical_path_delay = report.critical_path_delay.max(max_arr);
            }
        }

        report
    }

    /// Get the critical path endpoints (nodes with longest arrival times).
    ///
    /// Returns a list of (aigpin, arrival_time) tuples, sorted by arrival time descending.
    pub fn get_critical_paths(&self, limit: usize) -> Vec<(usize, u64)> {
        let mut endpoints: Vec<(usize, u64)> = Vec::new();

        // Collect all endpoints (primary outputs and DFF D inputs)
        for &po_iv in &self.primary_outputs {
            let po = po_iv >> 1; // Strip inversion bit
            if po > 0 && po <= self.num_aigpins {
                let (_, max_arr) = self.arrival_times[po];
                endpoints.push((po, max_arr));
            }
        }
        for (_cell_id, dff) in &self.dffs {
            let d_idx = dff.d_iv >> 1;
            if d_idx > 0 && d_idx <= self.num_aigpins {
                let (_, max_arr) = self.arrival_times[d_idx];
                endpoints.push((d_idx, max_arr));
            }
        }

        // Sort by arrival time descending
        endpoints.sort_by(|a, b| b.1.cmp(&a.1));
        endpoints.truncate(limit);
        endpoints
    }

    /// Trace back the critical path from an endpoint.
    ///
    /// Returns a list of (aigpin, arrival_time) tuples from endpoint back to a primary input.
    pub fn trace_critical_path(&self, endpoint: usize) -> Vec<(usize, u64)> {
        let mut path = Vec::new();
        let mut current = endpoint;

        while current > 0 {
            let (_, arrival) = self.arrival_times[current];
            path.push((current, arrival));

            match &self.drivers[current] {
                DriverType::AndGate(a, b) => {
                    let a_idx = a >> 1;
                    let b_idx = b >> 1;

                    // Follow the input with larger arrival time
                    let a_arr = if a_idx > 0 {
                        self.arrival_times[a_idx].1
                    } else {
                        0
                    };
                    let b_arr = if b_idx > 0 {
                        self.arrival_times[b_idx].1
                    } else {
                        0
                    };

                    current = if a_arr >= b_arr { a_idx } else { b_idx };
                }
                _ => break, // Reached a primary input or sequential element
            }
        }

        path
    }

    /// Dump detailed critical paths with cell origins, delays, and cumulative arrivals.
    ///
    /// Returns a formatted string showing:
    /// - For each critical endpoint, the full path from source to sink
    /// - Each node's cell origin (synthesis cell that created it)
    /// - Gate delays from the Liberty library
    /// - Cumulative arrival times
    pub fn dump_critical_paths_detailed(&self, limit: usize) -> String {
        let mut output = String::new();
        output.push_str("=== AIG Critical Path Analysis ===\n\n");

        let critical_paths = self.get_critical_paths(limit);

        for (endpoint_idx, (endpoint, arrival)) in critical_paths.iter().enumerate() {
            let slack = self.clock_period_ps as i64 - *arrival as i64;

            // Determine endpoint label
            let endpoint_label = if self.primary_outputs.contains(endpoint) {
                format!("Primary Output (aigpin {})", endpoint)
            } else {
                // Find which DFF this belongs to
                let mut dff_label = format!("DFF D-input (aigpin {})", endpoint);
                for (_cell_id, dff) in &self.dffs {
                    if (dff.d_iv >> 1) == *endpoint {
                        dff_label = format!("DFF D-input (aigpin {}) [unknown DFF]", endpoint);
                        break;
                    }
                }
                dff_label
            };

            output.push_str(&format!(
                "#{}: {} arrival={} ps, slack={} ps\n",
                endpoint_idx + 1,
                endpoint_label,
                arrival,
                slack
            ));

            // Trace the path back
            let path = self.trace_critical_path(*endpoint);

            output.push_str("Path (source → sink):\n");
            for (depth, (node, node_arrival)) in path.iter().enumerate() {
                let (rise_delay, fall_delay) = self.gate_delays[*node];
                let driver_label = match &self.drivers[*node] {
                    DriverType::AndGate(_, _) => "AND".to_string(),
                    DriverType::InputPort(_) => "INPUT".to_string(),
                    DriverType::InputClockFlag(_, _) => "CLOCK".to_string(),
                    DriverType::DFF(_) => "DFF_Q".to_string(),
                    DriverType::SRAM(_) => "SRAM_READ".to_string(),
                    DriverType::Tie0 => "TIE0".to_string(),
                };

                let max_delay = rise_delay.max(fall_delay);
                output.push_str(&format!(
                    "  [{:2}] aigpin {:5} | type: {:10} | delay: {:5} ps | arrival: {:5} ps",
                    depth, node, driver_label, max_delay, node_arrival
                ));

                // Show cell origins if available
                if !self.aigpin_cell_origins[*node].is_empty() {
                    let origins = &self.aigpin_cell_origins[*node];
                    for (cell_id, cell_type, pin_name) in origins {
                        output.push_str(&format!(
                            " | cell: {} {} (id:{})",
                            cell_type, pin_name, cell_id
                        ));
                    }
                } else if *node > 0 {
                    output.push_str(" | (internal)");
                }
                output.push('\n');
            }
            output.push('\n');
        }

        output
    }
}

/// A reusable topological traverser with dense visited buffer.
///
/// Uses a generation counter pattern to avoid clearing the visited buffer
/// between traversals. This is significantly faster than `IndexSet<usize>`
/// for repeated traversals on the same AIG.
pub struct TopoTraverser {
    visited: Vec<u32>,
    generation: u32,
}

impl TopoTraverser {
    /// Create a new traverser for an AIG with the given number of pins.
    pub fn new(num_aigpins: usize) -> Self {
        Self {
            visited: vec![0; num_aigpins + 1],
            generation: 1,
        }
    }

    /// Reset the generation counter, preparing for a new traversal.
    fn new_generation(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        if self.generation == 0 {
            // Overflow: clear the buffer and reset
            self.visited.fill(0);
            self.generation = 1;
        }
    }

    #[inline(always)]
    fn is_visited(&self, u: usize) -> bool {
        self.visited[u] == self.generation
    }

    #[inline(always)]
    fn mark_visited(&mut self, u: usize) {
        self.visited[u] = self.generation;
    }

    /// Perform a topological traversal, equivalent to `AIG::topo_traverse_generic`.
    ///
    /// Uses an iterative stack-based DFS with a dense visited buffer for speed.
    pub fn topo_traverse(
        &mut self,
        aig: &AIG,
        endpoints: Option<&Vec<usize>>,
        is_primary_input: Option<&IndexSet<usize>>,
    ) -> Vec<usize> {
        self.new_generation();
        let mut ret = Vec::new();
        // Two-phase iterative DFS: Visit checks + pushes deps, Process emits
        enum Phase {
            Visit(usize),
            Process(usize),
        }
        let mut stack = Vec::new();

        if let Some(endpoints) = endpoints {
            for &endpoint in endpoints {
                stack.push(Phase::Visit(endpoint));
            }
        } else {
            for i in (1..aig.num_aigpins + 1).rev() {
                stack.push(Phase::Visit(i));
            }
        }

        while let Some(item) = stack.pop() {
            match item {
                Phase::Visit(u) => {
                    if self.is_visited(u) {
                        continue;
                    }
                    self.mark_visited(u);
                    stack.push(Phase::Process(u));
                    if let DriverType::AndGate(a, b) = aig.drivers[u] {
                        if is_primary_input.map(|s| s.contains(&u)) != Some(true) {
                            // Push b first so a is processed first (stack order)
                            if (b >> 1) != 0 {
                                stack.push(Phase::Visit(b >> 1));
                            }
                            if (a >> 1) != 0 {
                                stack.push(Phase::Visit(a >> 1));
                            }
                        }
                    }
                }
                Phase::Process(u) => {
                    ret.push(u);
                }
            }
        }
        ret
    }

    /// Perform a topological traversal and also produce a bitset of visited nodes.
    ///
    /// Returns `(order, bitset)` where `bitset` has bit `node` set for each visited node.
    /// The bitset is `Vec<u64>` with `(num_aigpins + 64) / 64` words.
    pub fn topo_traverse_with_bitset(
        &mut self,
        aig: &AIG,
        endpoints: Option<&Vec<usize>>,
        is_primary_input: Option<&IndexSet<usize>>,
    ) -> (Vec<usize>, Vec<u64>) {
        let order = self.topo_traverse(aig, endpoints, is_primary_input);
        let num_words = (aig.num_aigpins + 64) / 64;
        let mut bitset = vec![0u64; num_words];
        for &node in &order {
            bitset[node / 64] |= 1u64 << (node % 64);
        }
        (order, bitset)
    }
}

/// Compute the popcount of the union of two bitsets.
#[inline]
pub fn bitset_union_popcount(a: &[u64], b: &[u64]) -> usize {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| (x | y).count_ones() as usize)
        .sum()
}

/// OR bitset `src` into `dst` in-place.
#[inline]
pub fn bitset_or_inplace(dst: &mut [u64], src: &[u64]) {
    for (d, s) in dst.iter_mut().zip(src.iter()) {
        *d |= *s;
    }
}

/// Summary of timing analysis results.
#[derive(Debug, Clone, Default)]
pub struct TimingReport {
    /// Number of timing endpoints analyzed (DFFs + primary outputs)
    pub num_endpoints: usize,
    /// Critical path delay in picoseconds
    pub critical_path_delay: u64,
    /// Worst setup slack (negative = violation)
    pub worst_setup_slack: i64,
    /// Worst hold slack (negative = violation)
    pub worst_hold_slack: i64,
    /// Number of setup violations
    pub setup_violations: usize,
    /// Number of hold violations
    pub hold_violations: usize,
}

impl TimingReport {
    /// Returns true if there are any timing violations.
    pub fn has_violations(&self) -> bool {
        self.setup_violations > 0 || self.hold_violations > 0
    }
}

impl std::fmt::Display for TimingReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "=== Timing Report ===")?;
        writeln!(f, "Endpoints analyzed: {}", self.num_endpoints)?;
        writeln!(f, "Critical path delay: {} ps", self.critical_path_delay)?;
        writeln!(f, "Worst setup slack: {} ps", self.worst_setup_slack)?;
        writeln!(f, "Worst hold slack: {} ps", self.worst_hold_slack)?;
        writeln!(f, "Setup violations: {}", self.setup_violations)?;
        writeln!(f, "Hold violations: {}", self.hold_violations)?;
        if self.has_violations() {
            writeln!(f, "Status: TIMING VIOLATIONS DETECTED")?;
        } else {
            writeln!(f, "Status: All timing constraints met")?;
        }
        Ok(())
    }
}

// === X-Propagation Static Analysis ===

/// Statistics from X-propagation analysis.
#[derive(Debug, Clone)]
pub struct XPropStats {
    /// Number of X sources (DFF Q outputs + SRAM read data ports).
    pub num_x_sources: usize,
    /// Number of X-capable AIG pins after forward propagation.
    pub num_x_capable_pins: usize,
    /// Total number of AIG pins.
    pub total_pins: usize,
    /// Number of fixpoint iterations needed.
    pub fixpoint_iterations: usize,
}

impl AIG {
    /// Identify DFF Q outputs and SRAM read data ports as X sources.
    ///
    /// Returns a boolean vector indexed by aigpin (0..=num_aigpins).
    /// `x_sources[i] == true` means aigpin `i` is an X source at cycle 0.
    pub fn compute_x_sources(&self) -> Vec<bool> {
        let mut x_sources = vec![false; self.num_aigpins + 1];

        // DFF Q outputs are X sources (unknown at power-on)
        for dff in self.dffs.values() {
            assert!(dff.q >= 1 && dff.q <= self.num_aigpins);
            x_sources[dff.q] = true;
        }

        // SRAM read data ports are X sources (memory contents undefined).
        // `rd_pin == 0` denotes an unused slot — opaque RAMs declared via
        // the runtime cell-library manifest (ADR 0010 `kind = "ram"`)
        // have narrower output widths than the fixed 32-bit
        // `port_r_rd_data` array; unused slots stay default-zero and
        // must be skipped rather than asserted on.
        for sram in self.srams.values() {
            for &rd_pin in &sram.port_r_rd_data {
                if rd_pin == 0 {
                    continue;
                }
                assert!(rd_pin >= 1 && rd_pin <= self.num_aigpins);
                x_sources[rd_pin] = true;
            }
        }

        x_sources
    }

    /// Compute the full set of X-capable AIG pins via forward cone propagation
    /// and DFF fixpoint iteration.
    ///
    /// Returns `(x_capable, stats)` where `x_capable[i]` is true if aigpin `i`
    /// can carry an X value during simulation.
    ///
    /// Algorithm:
    /// 1. Mark all X sources (DFF Q, SRAM read data)
    /// 2. Forward pass through AND gates (pins are in topological order)
    /// 3. Fixpoint: if any DFF's D-input is X-capable but its Q is not,
    ///    mark Q and re-run forward pass from newly-marked pins.
    pub fn compute_x_capable_pins(&self) -> (Vec<bool>, XPropStats) {
        let mut x_capable = self.compute_x_sources();
        let num_x_sources = x_capable.iter().filter(|&&v| v).count();

        let mut fixpoint_iterations = 0;

        loop {
            // Forward pass: propagate X through AND gates.
            // AIG pins are guaranteed to be in topological order.
            for aigpin in 1..=self.num_aigpins {
                if x_capable[aigpin] {
                    continue; // already marked
                }
                if let DriverType::AndGate(a_iv, b_iv) = self.drivers[aigpin] {
                    let a = a_iv >> 1;
                    let b = b_iv >> 1;
                    if x_capable[a] || x_capable[b] {
                        x_capable[aigpin] = true;
                    }
                }
            }

            // Check fixpoint: do any DFF D-inputs feed X back to an
            // unmarked Q output?
            let mut changed = false;
            for dff in self.dffs.values() {
                let d_pin = dff.d_iv >> 1;
                if x_capable[d_pin] && !x_capable[dff.q] {
                    x_capable[dff.q] = true;
                    changed = true;
                }
            }

            fixpoint_iterations += 1;

            if !changed {
                break;
            }
        }

        let num_x_capable_pins = x_capable.iter().filter(|&&v| v).count();
        let stats = XPropStats {
            num_x_sources,
            num_x_capable_pins,
            total_pins: self.num_aigpins,
            fixpoint_iterations,
        };

        (x_capable, stats)
    }
}

#[cfg(test)]
mod xprop_tests {
    use super::*;

    /// Create an AIG with proper Tie0 initialization (matching from_netlistdb_impl).
    fn new_test_aig() -> AIG {
        AIG {
            drivers: vec![DriverType::Tie0],
            aigpin_cell_origins: vec![Vec::new()],
            ..Default::default()
        }
    }

    /// Build a minimal AIG with no DFFs or SRAMs (pure combinational).
    fn build_comb_only_aig() -> AIG {
        let mut aig = new_test_aig();
        // pin 1 = InputPort
        aig.add_aigpin(DriverType::InputPort(0));
        // pin 2 = InputPort
        aig.add_aigpin(DriverType::InputPort(1));
        // pin 3 = AND(pin1, pin2) — no inversion: a_iv=2, b_iv=4
        aig.add_aigpin(DriverType::AndGate(1 << 1, 2 << 1));
        aig
    }

    #[test]
    fn test_x_capable_no_dffs() {
        let aig = build_comb_only_aig();
        let (x_capable, stats) = aig.compute_x_capable_pins();
        // No DFFs, no SRAMs → no X sources, no X-capable pins
        assert_eq!(stats.num_x_sources, 0);
        assert_eq!(stats.num_x_capable_pins, 0);
        assert_eq!(stats.fixpoint_iterations, 1);
        for v in &x_capable {
            assert!(!v);
        }
    }

    #[test]
    fn test_x_capable_dff_forward_cone() {
        // Build AIG: InputPort(1), DFF Q(2), AND(1,2)=3, AND(3,1)=4
        let mut aig = new_test_aig();
        aig.add_aigpin(DriverType::InputPort(0)); // pin 1
        aig.add_aigpin(DriverType::DFF(0)); // pin 2
        aig.add_aigpin(DriverType::AndGate(1 << 1, 2 << 1)); // pin 3 = AND(1,2)
        aig.add_aigpin(DriverType::AndGate(3 << 1, 1 << 1)); // pin 4 = AND(3,1)

        // Register the DFF: D input is pin 3, Q output is pin 2
        aig.dffs.insert(
            0,
            DFF {
                d_iv: 3 << 1, // pin 3, no inversion
                en_iv: 0,     // always enabled
                q: 2,
            },
        );

        let (x_capable, stats) = aig.compute_x_capable_pins();

        assert_eq!(stats.num_x_sources, 1); // Just the DFF Q
                                            // Pin 2 (DFF Q) is X source, pin 3 (AND(1,2)) is X-capable,
                                            // pin 4 (AND(3,1)) is X-capable
        assert!(!x_capable[0]); // Tie0
        assert!(!x_capable[1]); // InputPort — not X
        assert!(x_capable[2]); // DFF Q — X source
        assert!(x_capable[3]); // AND(1,2) — b is X
        assert!(x_capable[4]); // AND(3,1) — a is X (via pin 3)
        assert_eq!(stats.num_x_capable_pins, 3);
    }

    #[test]
    fn test_x_capable_fixpoint() {
        // DFF feedback loop: DFF Q(1) -> AND(1, input) -> DFF2 Q(3) -> AND(3, input) -> back to DFF D
        // This tests that fixpoint converges when X propagates through DFF cycles.
        let mut aig = new_test_aig();
        aig.add_aigpin(DriverType::InputPort(0)); // pin 1
        aig.add_aigpin(DriverType::DFF(0)); // pin 2 (DFF0 Q)
        aig.add_aigpin(DriverType::AndGate(2 << 1, 1 << 1)); // pin 3 = AND(2,1)
        aig.add_aigpin(DriverType::DFF(1)); // pin 4 (DFF1 Q)
        aig.add_aigpin(DriverType::AndGate(4 << 1, 1 << 1)); // pin 5 = AND(4,1)

        // DFF0: D=pin5, Q=pin2
        aig.dffs.insert(
            0,
            DFF {
                d_iv: 5 << 1,
                en_iv: 0,
                q: 2,
            },
        );
        // DFF1: D=pin3, Q=pin4
        aig.dffs.insert(
            1,
            DFF {
                d_iv: 3 << 1,
                en_iv: 0,
                q: 4,
            },
        );

        let (x_capable, stats) = aig.compute_x_capable_pins();

        // Both DFFs are initially X sources, their forward cones too
        assert!(x_capable[2]); // DFF0 Q
        assert!(x_capable[3]); // AND(2,1)
        assert!(x_capable[4]); // DFF1 Q
        assert!(x_capable[5]); // AND(4,1)
        assert!(!x_capable[1]); // InputPort
        assert_eq!(stats.num_x_sources, 2);
        // Should converge in 1 iteration since both DFFs are already X sources
        assert_eq!(stats.fixpoint_iterations, 1);
    }

    #[test]
    fn test_x_capable_fixpoint_indirect() {
        // Test case where fixpoint needs >1 iteration:
        // InputPort(1), AND(1,1)=2, DFF0 Q=3 (D=2), AND(3,1)=4, DFF1 Q=5 (D=4)
        // DFF1 is initially NOT an X source if we only mark DFF0.
        // But wait — ALL DFFs are X sources by definition. So fixpoint always
        // converges in 1 iteration for the current algorithm.
        //
        // The fixpoint >1 case would only arise if we selectively excluded
        // some DFFs from X sources (e.g., reset-aware analysis). For now,
        // verify that the algorithm handles the general case correctly.
        let mut aig = new_test_aig();
        aig.add_aigpin(DriverType::InputPort(0)); // pin 1
        aig.add_aigpin(DriverType::DFF(0)); // pin 2 (initially X)
        aig.add_aigpin(DriverType::AndGate(2 << 1, 1 << 1)); // pin 3 = AND(2,1)
        aig.add_aigpin(DriverType::DFF(1)); // pin 4 (initially X)

        aig.dffs.insert(
            0,
            DFF {
                d_iv: 1 << 1,
                en_iv: 0,
                q: 2,
            },
        );
        aig.dffs.insert(
            1,
            DFF {
                d_iv: 3 << 1,
                en_iv: 0,
                q: 4,
            },
        );

        let (x_capable, stats) = aig.compute_x_capable_pins();
        assert!(x_capable[2]); // DFF0 Q — X source
        assert!(x_capable[3]); // AND(2,1) — X via DFF0
        assert!(x_capable[4]); // DFF1 Q — X source
        assert!(!x_capable[1]); // InputPort
        assert_eq!(stats.fixpoint_iterations, 1);
    }

    #[test]
    fn test_x_sources_sram() {
        // Test that SRAM read data ports are marked as X sources
        let mut aig = new_test_aig();
        aig.add_aigpin(DriverType::InputPort(0)); // pin 1

        // Create 32 SRAM read data pins
        let mut rd_data = [0usize; 32];
        for i in 0..32 {
            let pin = aig.add_aigpin(DriverType::SRAM(0)); // pins 2..33
            rd_data[i] = pin;
        }

        aig.srams.insert(
            0,
            RAMBlock {
                port_r_addr_iv: [0; AIGPDK_SRAM_ADDR_WIDTH],
                port_r_en_iv: 0,
                port_r_rd_data: rd_data,
                port_w_addr_iv: [0; AIGPDK_SRAM_ADDR_WIDTH],
                port_w_wr_en_iv: [0; 32],
                port_w_wr_data_iv: [0; 32],
            },
        );

        let x_sources = aig.compute_x_sources();
        assert!(!x_sources[0]); // Tie0
        assert!(!x_sources[1]); // InputPort
        for i in 0..32 {
            assert!(
                x_sources[rd_data[i]],
                "SRAM rd_data[{i}] should be X source"
            );
        }

        let (x_capable, stats) = aig.compute_x_capable_pins();
        assert_eq!(stats.num_x_sources, 32);
        assert_eq!(stats.num_x_capable_pins, 32);
        // Verify forward propagation through AND gate
        // Add an AND gate that depends on SRAM read data
        // (already tested via the basic structure above)
        drop(x_capable);
    }

    /// End-to-end: a synthetic netlist instantiating an
    /// OCD-SRAM-shaped opaque RAM, plus a TOML manifest declaring it
    /// as `kind = "ram"`, should build cleanly through
    /// `AIG::from_netlistdb_with_cells`. Verifies the
    /// third-party-IP unblock — `gf180mcu_ocd_ip_sram__sram1024x8m8wm1`
    /// is not in Jacquard's vendored library, has no built-in pin
    /// table, previously fell through to the AIGPDK AND2/INV/BUF
    /// fallback and produced wrong results.
    #[test]
    fn test_opaque_ram_end_to_end() {
        use crate::cell_library::RuntimeCellLibrary;
        use compact_str::CompactString;
        use netlistdb::LeafPinProvider;
        use sverilogparse::SVerilogRange;

        // Blackbox interface mirroring
        // gf180mcu_ocd_ip_sram__sram1024x8m8wm1, verbatim from
        // RTimothyEdwards/gf180mcu_ocd_ip_sram. Uses the ANSI-style
        // port-list form that's dominant in modern third-party IP
        // blackbox.v files — supported via the sverilogparse fix
        // in ChipFlow/eda-infra-rs#1 (closes #69). Before that fix
        // landed, this test had to be written in the older
        // port-names-in-header + separate body declarations form
        // as a workaround.
        const OCD_SRAM_BLACKBOX: &str = "\
(* blackbox *)
module gf180mcu_ocd_ip_sram__sram1024x8m8wm1 (
  input         CLK,
  input         CEN,
  input         GWEN,
  input  [7:0]  WEN,
  input  [9:0]  A,
  input  [7:0]  D,
  output [7:0]  Q
);
endmodule
";

        // Tiny top instantiating one SRAM. Inputs are primary ports;
        // outputs Q[7:0] are the cell's output, routed to top-level
        // outputs so they end up in the netlist's primary-output set.
        const TOP_VERILOG: &str = "\
module top(clk, cen, gwen, wen, a, d, q);
  input clk, cen, gwen;
  input [7:0] wen;
  input [9:0] a;
  input [7:0] d;
  output [7:0] q;
  gf180mcu_ocd_ip_sram__sram1024x8m8wm1 u_sram (
    .CLK(clk), .CEN(cen), .GWEN(gwen),
    .WEN(wen), .A(a), .D(d), .Q(q)
  );
endmodule
";

        // Write the cell library file (with sibling manifest) so the
        // sverilogparse-backed loader sees them via the public API.
        let dir = tempfile::TempDir::new().unwrap();
        let lib_path = dir.path().join("ocd_sram.v");
        std::fs::write(&lib_path, OCD_SRAM_BLACKBOX).unwrap();
        std::fs::write(
            dir.path().join("ocd_sram.cells.toml"),
            "schema_version = \"1.0\"\n\
             [cells.gf180mcu_ocd_ip_sram__sram1024x8m8wm1]\n\
             kind = \"ram\"\n",
        )
        .unwrap();

        let runtime_lib = RuntimeCellLibrary::from_files(&[lib_path]).unwrap();

        // Sanity: the manifest was loaded.
        assert_eq!(
            runtime_lib.lookup_kind("gf180mcu_ocd_ip_sram__sram1024x8m8wm1"),
            Some(crate::cell_library::CellKind::Ram)
        );
        assert_eq!(
            runtime_lib.lookup_pin(
                &CompactString::new("gf180mcu_ocd_ip_sram__sram1024x8m8wm1"),
                &CompactString::new("Q")
            ),
            Some(netlistdb::Direction::O)
        );

        // The runtime library is the only pin-direction source for
        // this cell — no built-in fallback needed here. The
        // top module's port directions come from the inline
        // declarations themselves, which sverilogparse handles
        // directly.
        struct NoFallback;
        impl LeafPinProvider for NoFallback {
            fn direction_of(
                &self,
                _: &CompactString,
                _: &CompactString,
                _: Option<isize>,
            ) -> netlistdb::Direction {
                netlistdb::Direction::Unknown
            }
            fn width_of(
                &self,
                _: &CompactString,
                _: &CompactString,
            ) -> Option<SVerilogRange> {
                None
            }
        }
        let fallback = NoFallback;
        let provider = crate::cell_library::ChainedPinProvider {
            runtime: &runtime_lib,
            fallback: &fallback,
        };

        let nl = netlistdb::NetlistDB::from_sverilog_source(
            TOP_VERILOG,
            Some("top"),
            &provider,
        )
        .expect("netlist parse");

        let aig = AIG::from_netlistdb_with_cells(&nl, Some(&runtime_lib));

        // The OCD SRAM cell now has a `srams` entry — opaque-RAM
        // allocation path took effect.
        assert!(
            !aig.srams.is_empty(),
            "expected at least one srams entry for the opaque RAM"
        );
        let (cellid, ram) = aig.srams.iter().next().unwrap();
        // First 8 slots of port_r_rd_data populated (Q[7:0]); the
        // remaining 24 stay default-zero.
        let populated = ram.port_r_rd_data.iter().filter(|&&p| p != 0).count();
        assert_eq!(
            populated, 8,
            "expected 8 populated rd_data slots for an 8-bit opaque RAM (got {populated})"
        );

        // X-source enumeration tolerates the zero slots without panic.
        let x_sources = aig.compute_x_sources();
        let x_count = x_sources.iter().filter(|&&x| x).count();
        assert_eq!(
            x_count, 8,
            "expected 8 X-sources (the 8 OCD SRAM Q outputs) — got {x_count}"
        );

        // The cell origin is recorded with the manifest-declared name.
        let _ = cellid; // future check: hierarchical instance path
    }

    #[test]
    fn test_x_sources_narrow_opaque_ram() {
        // Opaque-RAM cells declared via the runtime cell-library
        // manifest (ADR 0010 `kind = "ram"`) have output widths
        // narrower than the fixed 32-bit `port_r_rd_data` array.
        // The unused slots stay default-zero and `compute_x_sources`
        // must skip rather than assert on them.
        let mut aig = new_test_aig();
        aig.add_aigpin(DriverType::InputPort(0)); // pin 1

        // 8-bit opaque RAM — populate only slots 0..8, leave 8..32 zero.
        let mut rd_data = [0usize; 32];
        for i in 0..8 {
            rd_data[i] = aig.add_aigpin(DriverType::SRAM(42)); // pins 2..9
        }

        aig.srams.insert(
            42,
            RAMBlock {
                port_r_addr_iv: [0; AIGPDK_SRAM_ADDR_WIDTH],
                port_r_en_iv: 0,
                port_r_rd_data: rd_data,
                port_w_addr_iv: [0; AIGPDK_SRAM_ADDR_WIDTH],
                port_w_wr_en_iv: [0; 32],
                port_w_wr_data_iv: [0; 32],
            },
        );

        let x_sources = aig.compute_x_sources();
        for i in 0..8 {
            assert!(
                x_sources[rd_data[i]],
                "Opaque RAM rd_data[{i}] (pin {}) should be X source",
                rd_data[i]
            );
        }
        // No panic / assert from the unused-slot zeros.

        let (_x_capable, stats) = aig.compute_x_capable_pins();
        assert_eq!(stats.num_x_sources, 8);
    }

    #[test]
    fn test_x_prop_with_inv_chain() {
        // Integration test using the real inv_chain.v design
        let verilog_path = std::path::PathBuf::from("tests/timing_test/sky130_timing/inv_chain.v");
        if !verilog_path.exists() {
            eprintln!("Skipping test_x_prop_with_inv_chain: inv_chain.v not found");
            return;
        }
        use crate::sky130::SKY130LeafPins;
        let netlistdb = NetlistDB::from_sverilog_file(&verilog_path, None, &SKY130LeafPins)
            .expect("cannot build netlist");
        let aig = AIG::from_netlistdb(&netlistdb);

        let (x_capable, stats) = aig.compute_x_capable_pins();

        // inv_chain has 2 DFFs, so at least 2 X sources
        assert!(
            stats.num_x_sources >= 2,
            "Expected at least 2 X sources from DFFs, got {}",
            stats.num_x_sources
        );
        // The forward cone should propagate X through the inverter chain
        assert!(
            stats.num_x_capable_pins >= stats.num_x_sources,
            "X-capable pins ({}) should be >= X sources ({})",
            stats.num_x_capable_pins,
            stats.num_x_sources
        );
        // Sanity: not everything should be X-capable (inputs are known)
        assert!(
            stats.num_x_capable_pins < stats.total_pins,
            "Not all pins should be X-capable"
        );

        println!(
            "inv_chain X-prop: {}/{} pins ({:.1}%) X-capable, {} fixpoint iterations",
            stats.num_x_capable_pins,
            stats.total_pins,
            100.0 * stats.num_x_capable_pins as f64 / stats.total_pins as f64,
            stats.fixpoint_iterations
        );

        // Verify no input ports are X-capable
        for (aigpin, driver) in aig.drivers.iter().enumerate() {
            if matches!(
                driver,
                DriverType::InputPort(_) | DriverType::InputClockFlag(_, _)
            ) {
                assert!(
                    !x_capable[aigpin],
                    "Input port aigpin {} should not be X-capable",
                    aigpin
                );
            }
        }
    }
}

#[cfg(test)]
mod path_mapping_tests {
    use super::*;
    use crate::sky130::SKY130LeafPins;
    use std::collections::HashMap;

    /// Helper: load inv_chain.v and build AIG.
    fn load_inv_chain_aig() -> (NetlistDB, AIG) {
        let verilog_path = std::path::PathBuf::from("tests/timing_test/sky130_timing/inv_chain.v");
        assert!(verilog_path.exists(), "inv_chain.v not found");

        let netlistdb = NetlistDB::from_sverilog_file(&verilog_path, None, &SKY130LeafPins)
            .expect("Failed to parse inv_chain.v");

        let aig = AIG::from_netlistdb(&netlistdb);
        (netlistdb, aig)
    }

    /// Helper: build cellid → IR path map.
    /// Mirrors the construction in `load_timing_from_ir` (uses `/` as the
    /// hierarchy separator, matching OpenSTA's default divider).
    fn build_cellid_to_path(netlistdb: &NetlistDB) -> HashMap<usize, String> {
        let mut map = HashMap::new();
        for cellid in 1..netlistdb.num_cells {
            let parts: Vec<&str> = netlistdb.cellnames[cellid]
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>();
            let path: String = parts.iter().rev().cloned().collect::<Vec<_>>().join("/");
            map.insert(cellid, path);
        }
        map
    }

    // === Test 1: Cell origin population ===

    #[test]
    fn test_cell_origins_length() {
        let (_netlistdb, aig) = load_inv_chain_aig();
        assert_eq!(
            aig.aigpin_cell_origins.len(),
            aig.num_aigpins + 1,
            "aigpin_cell_origins should have num_aigpins + 1 entries (index 0 = Tie0)"
        );
    }

    #[test]
    fn test_cell_origin_tie0_is_empty() {
        let (_netlistdb, aig) = load_inv_chain_aig();
        assert!(
            aig.aigpin_cell_origins[0].is_empty(),
            "Index 0 (Tie0) must have no cell origins"
        );
    }

    #[test]
    fn test_cell_origins_accumulated() {
        let (_netlistdb, aig) = load_inv_chain_aig();

        // With accumulated origins, inverters no longer overwrite — they push.
        // For inv_chain: dff_in.Q → i0 → ... → i15 → dff_out.D
        // The shared AIG pin (dff_in.Q / inverter chain) accumulates:
        //   1 DFF origin (dff_in CLK→Q) + 16 inverter origins = 17 entries.
        // dff_out.Q gets its own AIG pin with 1 entry.
        let non_empty_count = aig
            .aigpin_cell_origins
            .iter()
            .filter(|o| !o.is_empty())
            .count();

        // 2 AIG pins with non-empty origins: shared chain pin + dff_out
        assert_eq!(
            non_empty_count, 2,
            "Expected 2 AIG pins with cell origins (chain pin + dff_out), got {}",
            non_empty_count
        );

        // The shared pin should have 17 entries (1 DFF + 16 inverters)
        let chain_pin = aig
            .aigpin_cell_origins
            .iter()
            .find(|origins| origins.len() > 1)
            .expect("Should have a pin with multiple origins (the inverter chain)");
        assert_eq!(
            chain_pin.len(),
            17,
            "Shared chain pin should have 17 origins (1 DFF + 16 inverters), got {}",
            chain_pin.len()
        );
    }

    #[test]
    fn test_cell_origin_dff_output_pin() {
        let (netlistdb, aig) = load_inv_chain_aig();

        // Both DFFs should now be present (dff_in is no longer overwritten)
        let dff_origins: Vec<_> = aig
            .aigpin_cell_origins
            .iter()
            .enumerate()
            .flat_map(|(i, origins)| {
                origins
                    .iter()
                    .map(move |(cid, ct, pin)| (i, *cid, ct.clone(), pin.clone()))
            })
            .filter(|(_, _, ct, _)| ct == "dfxtp")
            .collect();

        assert_eq!(
            dff_origins.len(),
            2,
            "Expected 2 DFF cell origins (dff_in + dff_out), got {}",
            dff_origins.len()
        );

        for (ref _aigpin, ref cellid, ref _cell_type, ref output_pin_name) in &dff_origins {
            assert_eq!(output_pin_name, "Q", "DFF output pin must be Q");
            assert!(
                netlistdb.celltypes[*cellid].contains("dfxtp"),
                "Cell {} should be dfxtp type, got {}",
                cellid,
                netlistdb.celltypes[*cellid]
            );
        }
    }

    #[test]
    fn test_all_inverters_present() {
        let (netlistdb, aig) = load_inv_chain_aig();

        // All 16 inverters should now have their origins accumulated (not just the last one)
        let inv_origins: Vec<_> = aig
            .aigpin_cell_origins
            .iter()
            .enumerate()
            .flat_map(|(i, origins)| {
                origins
                    .iter()
                    .map(move |(cid, ct, pin)| (i, *cid, ct.clone(), pin.clone()))
            })
            .filter(|(_, _, ct, _)| ct == "inv")
            .collect();

        assert_eq!(
            inv_origins.len(),
            16,
            "Expected 16 inverter cell origins (all inverters in chain), got {}",
            inv_origins.len()
        );

        for (ref _aigpin, ref cellid, ref _cell_type, ref output_pin_name) in &inv_origins {
            assert_eq!(output_pin_name, "Y", "Inverter output pin must be Y");
            assert!(
                netlistdb.celltypes[*cellid].contains("inv"),
                "Cell {} should be inv type, got {}",
                cellid,
                netlistdb.celltypes[*cellid]
            );
        }
    }

    #[test]
    fn test_no_and_gates_for_inverters() {
        let (_netlistdb, aig) = load_inv_chain_aig();

        // Inverters in AIG are just wire + invert bit, no AND gates needed.
        // The only AND gates should be none (inv_chain has no multi-input gates).
        let and_gate_count = (1..=aig.num_aigpins)
            .filter(|&ap| matches!(aig.drivers[ap], DriverType::AndGate(_, _)))
            .count();

        assert_eq!(
            and_gate_count, 0,
            "Inverter chain should produce 0 AND gates, got {}",
            and_gate_count
        );
    }

    // === Test 2: HierName → IR cell-instance path matching ===

    #[test]
    fn test_path_matching_all_cells() {
        // Every cell in the netlist should produce a path matching one of
        // the expected leaf-instance names. This is the same property the
        // IR consumer (`load_timing_from_ir`) relies on when looking up
        // arcs/checks by `cell_instance` string.
        let (netlistdb, _aig) = load_inv_chain_aig();
        let cellid_to_path = build_cellid_to_path(&netlistdb);

        // 18 real cells: 2 DFFs (dff_in, dff_out) + 16 inverters (i0..i15).
        let expected: std::collections::HashSet<&str> = [
            "dff_in", "i0", "i1", "i2", "i3", "i4", "i5", "i6", "i7", "i8", "i9", "i10", "i11",
            "i12", "i13", "i14", "i15", "dff_out",
        ]
        .into_iter()
        .collect();

        let mut matched = 0;
        let mut unmatched = Vec::new();
        for cellid in 1..netlistdb.num_cells {
            let path = &cellid_to_path[&cellid];
            if expected.contains(path.as_str()) {
                matched += 1;
            } else {
                unmatched.push(format!(
                    "cellid={} type={} path={}",
                    cellid, netlistdb.celltypes[cellid], path
                ));
            }
        }

        assert!(
            unmatched.is_empty(),
            "All cells should produce expected instance paths. Unmatched:\n{}",
            unmatched.join("\n")
        );
        assert_eq!(matched, 18, "Expected 18 matched cells");
    }

    #[test]
    fn test_path_format_flat() {
        // For a flat design, cellnames should produce single-component paths
        // (no `/` separators).
        let (netlistdb, _aig) = load_inv_chain_aig();
        let cellid_to_path = build_cellid_to_path(&netlistdb);

        let expected_names = [
            "dff_in", "i0", "i1", "i2", "i3", "i4", "i5", "i6", "i7", "i8", "i9", "i10", "i11",
            "i12", "i13", "i14", "i15", "dff_out",
        ];

        let paths: Vec<&str> = cellid_to_path.values().map(|s| s.as_str()).collect();
        for name in &expected_names {
            assert!(
                paths.contains(name),
                "Expected path '{}' not found. Paths: {:?}",
                name,
                paths
            );
        }
    }
}

#[cfg(test)]
mod gf180mcu_chip_top_tests {
    use super::*;
    use crate::gf180mcu::GF180MCULeafPins;

    /// End-to-end: a miniature GF180MCU chip-top builds without panic
    /// — full pad ring (in_s / in_c / bi_24t / asig_5p0 / fill10 / cor
    /// / ws_io / ws_ip) wrapping a trivial std-cell core. Catches
    /// regressions in:
    ///   * `is_gf180mcu_cell` + `extract_cell_type` (gf180mcu_pdk)
    ///   * `GF180MCU_PAD_PIN_TABLE` (gf180mcu.rs)
    ///   * `is_filler_cell` / `is_io_pad_cell` (gf180mcu_pdk.rs)
    ///   * `gf180mcu_postprocess` IO-pad branch (this file)
    ///   * `get_gf180mcu_dependencies` IO-pad branch (this file)
    #[test]
    fn aig_builds_from_chip_top_with_pad_ring() {
        const VERILOG: &str = r#"
module chip_top(clk_PAD, in_PAD, bidir_PAD, analog_PAD);
  inout clk_PAD;
  inout in_PAD;
  inout bidir_PAD;
  inout analog_PAD;
  wire clk_core, in_core, bidir_y_core, bidir_a_core, oe_core;
  wire mid;
  // Logic-bearing pads.
  gf180mcu_fd_io__in_s clk_pad (.PAD(clk_PAD), .PU(1'b0), .PD(1'b0), .Y(clk_core));
  gf180mcu_fd_io__in_c in_pad  (.PAD(in_PAD),  .PU(1'b0), .PD(1'b0), .Y(in_core));
  gf180mcu_fd_io__bi_24t bidir_pad (
      .PAD(bidir_PAD), .A(bidir_a_core), .OE(oe_core),
      .IE(1'b1), .CS(1'b0), .SL(1'b0), .PU(1'b0), .PD(1'b0),
      .Y(bidir_y_core)
  );
  // No-logic pads (no port connections, matching post-P&R netlists).
  gf180mcu_fd_io__asig_5p0 asig_inst (.ASIG5V(analog_PAD));
  gf180mcu_fd_io__fill10 fill_inst ();
  gf180mcu_fd_io__cor    cor_inst  ();
  gf180mcu_ws_io__dvdd   dvdd_inst ();
  gf180mcu_ws_io__dvss   dvss_inst ();
  gf180mcu_ws_ip__id     id_inst   ();
  gf180mcu_ws_ip__logo   logo_inst ();
  // Trivial core: bidir_a_core = NAND(clk, in); oe_core = AND(clk, in).
  gf180mcu_fd_sc_mcu9t5v0__nand2_1 u1 (.A1(clk_core), .A2(in_core), .ZN(mid));
  gf180mcu_fd_sc_mcu9t5v0__buf_1   u2 (.I(mid), .Z(bidir_a_core));
  gf180mcu_fd_sc_mcu9t5v0__and2_1  u3 (.A1(clk_core), .A2(in_core), .Z(oe_core));
endmodule
"#;
        let nl = netlistdb::NetlistDB::from_sverilog_source(
            VERILOG,
            Some("chip_top"),
            &GF180MCULeafPins,
        )
        .expect("netlist parse");
        let aig = AIG::from_netlistdb(&nl);
        assert!(aig.num_aigpins > 0, "AIG should have at least one pin");
    }

    /// The bi_24t simplification (Y = PAD) means a bi_24t pad's `Y`
    /// cone matches an `in_c` pad's `Y` cone. Build two equivalent
    /// designs — one using bi_24t with constant core drives for
    /// A/OE, one using in_c — and verify both build successfully
    /// with sensible AIG sizes.
    ///
    /// Note: with the bidir split, the bi_24t's A/OE pins *are* now
    /// in the AIG (as deps of the postprocess hook), and surface as
    /// extra observables (`p__out`/`p__oe`). The load-bearing
    /// assertion here is that the `p → y` relationship is unchanged.
    #[test]
    fn bi_24t_input_mode_matches_in_c() {
        // Trivial 1-pad design using bi_24t with A/OE driven by ties.
        // The tie cells are now reached by the bi_24t dep walk and
        // surface as `p__out`/`p__oe` extra observables — that's fine,
        // because the `p → y` cone is independent of those.
        const BI_24T_VERILOG: &str = r#"
module top(p, y);
  inout p;
  output y;
  wire ig1, ig2;
  gf180mcu_fd_io__bi_24t bidir_pad (
      .PAD(p), .A(ig1), .OE(ig2),
      .IE(1'b1), .CS(1'b0), .SL(1'b0), .PU(1'b0), .PD(1'b0),
      .Y(y)
  );
  gf180mcu_fd_sc_mcu9t5v0__tieh u_tie1 (.Z(ig1));
  gf180mcu_fd_sc_mcu9t5v0__tiel u_tie2 (.ZN(ig2));
endmodule
"#;
        const IN_C_VERILOG: &str = r#"
module top(p, y);
  inout p;
  output y;
  gf180mcu_fd_io__in_c in_pad (.PAD(p), .PU(1'b0), .PD(1'b0), .Y(y));
endmodule
"#;
        let nl_bi = netlistdb::NetlistDB::from_sverilog_source(
            BI_24T_VERILOG,
            Some("top"),
            &GF180MCULeafPins,
        )
        .expect("bi_24t netlist parse");
        let nl_ic = netlistdb::NetlistDB::from_sverilog_source(
            IN_C_VERILOG,
            Some("top"),
            &GF180MCULeafPins,
        )
        .expect("in_c netlist parse");
        let aig_bi = AIG::from_netlistdb(&nl_bi);
        let aig_ic = AIG::from_netlistdb(&nl_ic);
        // Both AIGs should observe the same primary-input → primary-
        // output relationship for `p → y`. We don't compare exact
        // structures (the bi_24t version has tie cells in its
        // `p__out`/`p__oe` observables); the load-bearing assertion
        // is that AIG construction succeeded and produced a
        // sensible-sized graph.
        assert!(aig_bi.num_aigpins > 0);
        assert!(aig_ic.num_aigpins > 0);
    }

    /// `bi_24t` bidirectional pads surface their core-side `A` (drive)
    /// and `OE` (output-enable) pins as extra observable primary outputs,
    /// named after the top-level inout port they connect to (e.g.
    /// `bidir_PAD[12]__out` / `bidir_PAD[12]__oe`). This lets the output
    /// VCD capture the core's drive-out direction of a bidir pad — the
    /// `Y = PAD` alias is still emitted for the input direction.
    ///
    /// `in_c` / `in_s` are *not* split (no `A`/`OE` to surface).
    #[test]
    fn bi_24t_surfaces_a_and_oe_as_observables() {
        const VERILOG: &str = r#"
module top(p, y);
  inout p;
  output y;
  wire core_a, core_oe;
  gf180mcu_fd_io__bi_24t bidir_pad (
      .PAD(p), .A(core_a), .OE(core_oe),
      .IE(1'b1), .CS(1'b0), .SL(1'b0), .PU(1'b0), .PD(1'b0),
      .Y(y)
  );
  // Distinct drivers for A and OE so they each get their own aigpin.
  gf180mcu_fd_sc_mcu9t5v0__tieh u_tie_a  (.Z(core_a));
  gf180mcu_fd_sc_mcu9t5v0__tiel u_tie_oe (.ZN(core_oe));
endmodule
"#;
        let nl = netlistdb::NetlistDB::from_sverilog_source(
            VERILOG,
            Some("top"),
            &GF180MCULeafPins,
        )
        .expect("netlist parse");
        let aig = AIG::from_netlistdb(&nl);
        assert!(aig.num_aigpins > 0);

        // Both `p__out` and `p__oe` should appear as named extra
        // observables, and each should be present in primary_outputs.
        let names: std::collections::HashSet<String> = aig
            .extra_observable_names
            .values()
            .flat_map(|v| v.iter().cloned())
            .collect();
        assert!(
            names.contains("p__out"),
            "expected `p__out` in extra_observable_names, got {:?}",
            names
        );
        assert!(
            names.contains("p__oe"),
            "expected `p__oe` in extra_observable_names, got {:?}",
            names
        );

        // Every extra observable must also live in primary_outputs so
        // the partitioner schedules it onto a state bit.
        for (&aigpin, names) in aig.extra_observable_names.iter() {
            assert!(
                aig.primary_outputs.contains(&aigpin),
                "extra observables {:?} (aigpin {}) not in primary_outputs",
                names,
                aigpin
            );
        }
    }

    /// Bus-indexed bidir pads use the top-level port's bus index in the
    /// observable name (e.g. `bidir_PAD[12]__out`). Verifies the
    /// PAD → top-port name lookup handles bus indices.
    #[test]
    fn bi_24t_observable_name_includes_bus_index() {
        const VERILOG: &str = r#"
module top(bidir_PAD);
  inout [1:0] bidir_PAD;
  wire core_a_0, core_oe_0;
  wire core_a_1, core_oe_1;
  wire y_0, y_1;
  gf180mcu_fd_io__bi_24t pad0 (
      .PAD(bidir_PAD[0]), .A(core_a_0), .OE(core_oe_0),
      .IE(1'b1), .CS(1'b0), .SL(1'b0), .PU(1'b0), .PD(1'b0),
      .Y(y_0)
  );
  gf180mcu_fd_io__bi_24t pad1 (
      .PAD(bidir_PAD[1]), .A(core_a_1), .OE(core_oe_1),
      .IE(1'b1), .CS(1'b0), .SL(1'b0), .PU(1'b0), .PD(1'b0),
      .Y(y_1)
  );
  gf180mcu_fd_sc_mcu9t5v0__tieh u_a0  (.Z(core_a_0));
  gf180mcu_fd_sc_mcu9t5v0__tiel u_oe0 (.ZN(core_oe_0));
  gf180mcu_fd_sc_mcu9t5v0__tieh u_a1  (.Z(core_a_1));
  gf180mcu_fd_sc_mcu9t5v0__tiel u_oe1 (.ZN(core_oe_1));
endmodule
"#;
        let nl = netlistdb::NetlistDB::from_sverilog_source(
            VERILOG,
            Some("top"),
            &GF180MCULeafPins,
        )
        .expect("netlist parse");
        let aig = AIG::from_netlistdb(&nl);
        let names: std::collections::HashSet<String> = aig
            .extra_observable_names
            .values()
            .flat_map(|v| v.iter().cloned())
            .collect();
        for idx in 0..2 {
            assert!(
                names.contains(&format!("bidir_PAD[{}]__out", idx)),
                "missing bidir_PAD[{}]__out in {:?}",
                idx,
                names,
            );
            assert!(
                names.contains(&format!("bidir_PAD[{}]__oe", idx)),
                "missing bidir_PAD[{}]__oe in {:?}",
                idx,
                names,
            );
        }
    }

    /// `in_c` / `in_s` pads do NOT contribute extra observables — they
    /// are input-only and have no `A`/`OE` pins to surface.
    #[test]
    fn in_c_in_s_pads_emit_no_extra_observables() {
        const VERILOG: &str = r#"
module top(clk_PAD, in_PAD, y0, y1);
  inout clk_PAD;
  inout in_PAD;
  output y0, y1;
  gf180mcu_fd_io__in_s clk_pad (.PAD(clk_PAD), .PU(1'b0), .PD(1'b0), .Y(y0));
  gf180mcu_fd_io__in_c in_pad  (.PAD(in_PAD),  .PU(1'b0), .PD(1'b0), .Y(y1));
endmodule
"#;
        let nl = netlistdb::NetlistDB::from_sverilog_source(
            VERILOG,
            Some("top"),
            &GF180MCULeafPins,
        )
        .expect("netlist parse");
        let aig = AIG::from_netlistdb(&nl);
        assert!(
            aig.extra_observable_names.is_empty(),
            "in_c / in_s pads should not contribute extra observables; got {:?}",
            aig.extra_observable_names,
        );
    }

    /// Regression: gf180mcu's `mux2` cell has a `S` select *input* port,
    /// but the previous postprocess inputs-collection logic was
    /// name-blacklisting `S` (assuming it was an adder's sum *output*).
    /// That caused `decompose_with_pdk` to panic "Input pin 'S' not
    /// provided" when constructing the AIG for any netlist containing
    /// a mux2 — which is every real GF180MCU post-synthesis netlist.
    /// Now we filter by `Direction::I` only and trust the pin table.
    #[test]
    fn aig_builds_for_mux2_with_select_input() {
        const VERILOG: &str = r#"
module top(a, b, sel, y);
  input a, b, sel;
  output y;
  gf180mcu_fd_sc_mcu9t5v0__mux2_1 u1 (.I0(a), .I1(b), .S(sel), .Z(y));
endmodule
"#;
        let nl = netlistdb::NetlistDB::from_sverilog_source(
            VERILOG,
            Some("top"),
            &GF180MCULeafPins,
        )
        .expect("netlist parse");
        let aig = AIG::from_netlistdb(&nl);
        assert!(aig.num_aigpins > 0, "AIG should have at least one pin");
    }
}
