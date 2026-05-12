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
//! Per the GF180MCU enablement plan (Phase 4-pre), this module
//! exists to make the PDK-neutral surface explicit, so GF180MCU
//! (and future PDKs) can call into it without depending on the
//! sky130_pdk module name. The underlying definitions still live
//! in [`crate::sky130_pdk`] — physical relocation is deferred to
//! a follow-up cleanup pass.

pub use crate::sky130_pdk::{
    parse_functional_model, parse_udp, BehavioralGate, BehavioralModel, UdpModel, UdpRow,
};

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
