// SPDX-FileCopyrightText: Copyright (c) 2026 ChipFlow Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! GF180MCU cell classification and AIG decomposition.
//!
//! Sibling of [`crate::sky130_pdk`]. Owns:
//! - sequential/tie/multi-output classification
//! - behavioral model parsing for the vendored cell models in
//!   `vendor/gf180mcu_fd_sc_mcu7t5v0/` and
//!   `vendor/gf180mcu_fd_sc_mcu9t5v0/`
//! - AIG decomposition rules
//!
//! Implementation phases land per `docs/plans/gf180mcu-enablement.md`.
//! This module is currently a skeleton (Phase 0); classification lands
//! in Phase 3, decomposition in Phase 4.

#![allow(unused)]
