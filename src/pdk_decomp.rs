// SPDX-FileCopyrightText: Copyright (c) 2024 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-FileCopyrightText: Copyright (c) 2026 ChipFlow Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! PDK-neutral behavioral parsing and AIG decomposition primitives.
//!
//! The `.functional.v` cell-model files shipped by open-source PDKs
//! (SKY130, GF180MCU, and others that follow the same OpenLane /
//! OpenROAD ecosystem conventions) use a small fixed grammar:
//!
//! - One `module <name>( <ports> );` declaration.
//! - `input`/`output` port-direction lines.
//! - A topologically ordered sequence of Verilog gate-primitive
//!   instantiations (`not`, `buf`, `and`, `or`, `nand`, `nor`,
//!   `xor`, `xnor`) plus optional UDP instantiations whose names
//!   are PDK-specific (e.g. `sky130_fd_sc_hd__udp_*`,
//!   `UDP_GF018hv5v_mcu_sc7_*`).
//!
//! The parser is fully prefix-agnostic; UDP entries are surfaced
//! verbatim and the PDK-specific decomposition layer routes them
//! to PDK-specific UDP handlers.
//!
//! This module owns the PDK-neutral primitives that both `sky130_pdk`
//! and `gf180mcu_pdk` share:
//!
//! - The parser entry points (`parse_functional_model`, `parse_udp`)
//!   and their AST types (`BehavioralGate`, `BehavioralModel`,
//!   `UdpRow`, `UdpModel`) — re-exported from `sky130_pdk` until
//!   those parse routines are physically relocated.
//! - The AIG-builder helpers: `WireVal`, `GATE_MARKER`,
//!   `build_chain_gate`, `build_xor_chain`, `build_udp_aig`,
//!   `finalize_decomp_result`.
//! - The `DecompResult` type returned by every PDK's `decompose_*`
//!   entry point.
//!
//! PDK-specific lookup structs (sky130's fixed-field `CellInputs`,
//! gf180's `HashMap<String, usize>`) deliberately stay in their
//! respective `*_pdk` modules — they aren't actually shared.

use std::collections::HashMap;

// Parser AST + entry points still live in sky130_pdk.rs for now;
// re-export them here so callers get a single neutral surface.
pub use crate::sky130_pdk::{
    parse_functional_model, parse_udp, BehavioralGate, BehavioralModel, UdpModel, UdpRow,
};

// ============================================================================
// DecompResult — the universal output type
// ============================================================================

/// Result of decomposing a cell into AIG operations.
///
/// The decomposition produces a sequence of AND gates that must be built
/// in order, where later gates can reference earlier ones.
#[derive(Debug, Clone)]
pub struct DecompResult {
    /// Sequence of AND gate operations to build.
    /// Each entry is (input_a_iv, input_b_iv) where the lower bit is inversion.
    /// References to earlier gates use negative indices (-1 = first gate output, etc.)
    pub and_gates: Vec<(i64, i64)>,
    /// Index of the final output (-1 = first gate, -2 = second gate, etc.)
    /// Positive values reference original inputs.
    pub output_idx: i64,
    /// Whether to invert the final output
    pub output_inverted: bool,
}

// ============================================================================
// WireVal — internal builder state
// ============================================================================

/// Tagged value for tracking what kind of thing a wire holds during
/// behavioral-model decomposition. Either an AIG pin (real input or
/// AND gate we built (by gate index).
///
/// `pub(crate)` so sibling PDK modules can construct AIG sub-circuits
/// through the same primitives.
#[derive(Clone, Copy, Debug)]
pub(crate) enum WireVal {
    /// An AIG pin with inversion bit (aigpin_iv). Bit 0 = inverted.
    AigPin(usize),
    /// Constant value
    Const(bool),
}

impl WireVal {
    /// Get the aigpin_iv value, creating const-0 = AigPin(0) convention.
    pub(crate) fn as_aigpin_iv(self) -> i64 {
        match self {
            WireVal::AigPin(iv) => iv as i64,
            WireVal::Const(false) => 0, // const-0
            WireVal::Const(true) => 1,  // const-1
        }
    }

    /// Invert this wire value.
    pub(crate) fn inverted(self) -> Self {
        match self {
            WireVal::AigPin(iv) => WireVal::AigPin(iv ^ 1),
            WireVal::Const(v) => WireVal::Const(!v),
        }
    }
}

// ============================================================================
// AIG construction helpers (GATE_MARKER encoding)
// ============================================================================

/// Marker bit to distinguish gate references from pin references.
/// Gate outputs use bit 30 set. This limits us to ~500M gates (more than enough).
pub(crate) const GATE_MARKER: usize = 1 << 30;

/// Check if an aigpin_iv value is a gate reference.
fn is_gate_ref(aigpin_iv: usize) -> bool {
    aigpin_iv & GATE_MARKER != 0
}

/// Extract gate index from a gate-reference aigpin_iv.
fn gate_ref_index(aigpin_iv: usize) -> usize {
    (aigpin_iv & !GATE_MARKER & !1) >> 1
}

/// Build an AND/NAND/OR/NOR chain over N inputs.
///
/// For AND/NAND: compute AND of all inputs, optionally invert at the end.
/// For OR/NOR: invert all inputs, AND them, optionally invert at the end.
///   OR(a,b,c) = NOT(AND(NOT a, NOT b, NOT c))
///   NOR(a,b,c) = AND(NOT a, NOT b, NOT c)
pub(crate) fn build_chain_gate(
    inputs: &[WireVal],
    invert_inputs: bool,
    invert_output: bool,
    and_gates: &mut Vec<(i64, i64)>,
) -> WireVal {
    assert!(inputs.len() >= 2, "Gate must have at least 2 inputs");

    let inputs: Vec<WireVal> = if invert_inputs {
        inputs.iter().map(|v| v.inverted()).collect()
    } else {
        inputs.to_vec()
    };

    // Chain 2-input AND gates
    let mut accum = inputs[0];
    for input in &inputs[1..] {
        let a_ref = accum.as_aigpin_iv();
        let b_ref = input.as_aigpin_iv();
        and_gates.push((a_ref, b_ref));
        let gate_idx = and_gates.len() - 1;
        accum = WireVal::AigPin(GATE_MARKER | (gate_idx << 1));
    }

    if invert_output {
        accum.inverted()
    } else {
        accum
    }
}

/// Build a 2-input XOR: A ^ B = !(!( A & !B) & !(!A & B))
fn build_xor_2(a: WireVal, b: WireVal, and_gates: &mut Vec<(i64, i64)>) -> WireVal {
    let a_iv = a.as_aigpin_iv();
    let b_iv = b.as_aigpin_iv();
    let a_inv_iv = a.inverted().as_aigpin_iv();
    let b_inv_iv = b.inverted().as_aigpin_iv();

    // gate0: A & !B
    and_gates.push((a_iv, b_inv_iv));
    let g0 = and_gates.len() - 1;
    let g0_val = WireVal::AigPin(GATE_MARKER | (g0 << 1));

    // gate1: !A & B
    and_gates.push((a_inv_iv, b_iv));
    let g1 = and_gates.len() - 1;
    let g1_val = WireVal::AigPin(GATE_MARKER | (g1 << 1));

    // gate2: !(A & !B) & !(!A & B)  -- this is XNOR, inverted gives XOR
    let g0_inv_iv = g0_val.inverted().as_aigpin_iv();
    let g1_inv_iv = g1_val.inverted().as_aigpin_iv();
    and_gates.push((g0_inv_iv, g1_inv_iv));
    let g2 = and_gates.len() - 1;
    // XOR = NOT(gate2), so return inverted
    WireVal::AigPin(GATE_MARKER | (g2 << 1) | 1)
}

/// Build XOR/XNOR chain for multi-input gates.
pub(crate) fn build_xor_chain(
    inputs: &[WireVal],
    invert_output: bool,
    and_gates: &mut Vec<(i64, i64)>,
) -> WireVal {
    assert!(inputs.len() >= 2);

    let mut accum = inputs[0];
    for input in &inputs[1..] {
        accum = build_xor_2(accum, *input, and_gates);
    }

    if invert_output {
        accum.inverted()
    } else {
        accum
    }
}

/// Build AIG for a UDP instantiation by converting truth table to sum-of-products.
///
/// `pub(crate)` so sibling PDK modules (e.g. `gf180mcu_pdk::decompose_with_pdk`)
/// can route their own UDP gate-type prefixes through the same SOP builder.
pub(crate) fn build_udp_aig(
    gate: &BehavioralGate,
    wires: &HashMap<String, WireVal>,
    udps: &HashMap<String, UdpModel>,
    and_gates: &mut Vec<(i64, i64)>,
) -> WireVal {
    let udp_name = &gate.gate_type;
    let udp = udps
        .get(udp_name)
        .unwrap_or_else(|| panic!("UDP '{}' not found in loaded models", udp_name));

    // Get input wire values
    let input_vals: Vec<WireVal> = gate
        .inputs
        .iter()
        .map(|name| {
            wires
                .get(name)
                .copied()
                .unwrap_or_else(|| panic!("Unknown wire '{}' in UDP '{}'", name, udp_name))
        })
        .collect();

    assert_eq!(
        input_vals.len(),
        udp.inputs.len(),
        "UDP '{}' expects {} inputs, got {}",
        udp_name,
        udp.inputs.len(),
        input_vals.len()
    );

    // Build sum-of-products from truth table rows where output=1
    // Each row with output=1 becomes a product (AND) term.
    // Product terms are ORed together.
    //
    // For rows where output=0, we don't need to do anything explicitly.
    // Don't-care (?) inputs are omitted from the product term.

    let one_rows: Vec<&UdpRow> = udp.rows.iter().filter(|r| r.output).collect();

    if one_rows.is_empty() {
        // Output is always 0
        return WireVal::Const(false);
    }

    // Build each product term
    let mut product_terms: Vec<WireVal> = Vec::new();

    for row in &one_rows {
        // Collect non-don't-care inputs for this product term
        let mut term_inputs: Vec<WireVal> = Vec::new();
        for (i, pattern) in row.inputs.iter().enumerate() {
            match pattern {
                Some(true) => term_inputs.push(input_vals[i]),
                Some(false) => term_inputs.push(input_vals[i].inverted()),
                None => {} // don't-care - omit from product
            }
        }

        if term_inputs.is_empty() {
            // All inputs are don't-care: output is unconditionally 1
            return WireVal::Const(true);
        }

        if term_inputs.len() == 1 {
            product_terms.push(term_inputs[0]);
        } else {
            // Build AND chain for this product term
            let product = build_chain_gate(&term_inputs, false, false, and_gates);
            product_terms.push(product);
        }
    }

    if product_terms.len() == 1 {
        return product_terms[0];
    }

    // OR the product terms: OR(a,b,...) = NOT(AND(NOT a, NOT b, ...))
    build_chain_gate(&product_terms, true, true, and_gates)
}

// ============================================================================
// DecompResult conversion: GATE_MARKER encoding -> standard negative-index encoding
// ============================================================================

/// Post-process a DecompResult built with GATE_MARKER encoding to use
/// standard negative-index encoding for the and_gates references.
pub(crate) fn finalize_decomp_result(and_gates: Vec<(i64, i64)>, output: WireVal) -> DecompResult {
    // Convert gate references in and_gates from GATE_MARKER to negative indices
    let converted_gates: Vec<(i64, i64)> = and_gates
        .iter()
        .map(|(a, b)| (convert_ref_to_standard(*a), convert_ref_to_standard(*b)))
        .collect();

    match output {
        WireVal::AigPin(iv) if is_gate_ref(iv) => {
            let gate_idx = gate_ref_index(iv);
            let inverted = (iv & 1) != 0;
            DecompResult {
                and_gates: converted_gates,
                output_idx: -(gate_idx as i64) - 1,
                output_inverted: inverted,
            }
        }
        WireVal::AigPin(iv) => {
            let pin_idx = iv >> 1;
            let inverted = (iv & 1) != 0;
            DecompResult {
                and_gates: converted_gates,
                output_idx: pin_idx as i64,
                output_inverted: inverted,
            }
        }
        WireVal::Const(v) => DecompResult {
            and_gates: converted_gates,
            output_idx: 0,
            output_inverted: v,
        },
    }
}

/// Convert a single reference value from GATE_MARKER encoding to standard.
fn convert_ref_to_standard(ref_val: i64) -> i64 {
    let uval = ref_val as usize;
    if is_gate_ref(uval) {
        let gate_idx = gate_ref_index(uval);
        let inverted = (uval & 1) != 0;
        let base = -((gate_idx as i64) * 2 + 1);
        if inverted {
            base ^ 1
        } else {
            base
        }
    } else {
        ref_val
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sentinel: a regression catching any future change that breaks
    /// PDK-neutrality of the behavioural parser. Mirrors the recon
    /// tests in `gf180mcu_pdk::tests` but goes through the neutral
    /// re-export path callers are expected to use.
    #[test]
    fn parser_reachable_via_neutral_module() {
        let src = "module tiny( A, Y );\ninput A;\noutput Y;\n\tnot u(Y, A);\nendmodule\n";
        let m: BehavioralModel = parse_functional_model(src).expect("parse");
        assert_eq!(m.module_name, "tiny");
        assert_eq!(m.gates.len(), 1);
        assert_eq!(m.gates[0].gate_type, "not");
    }
}
