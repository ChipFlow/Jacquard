// SPDX-FileCopyrightText: Copyright (c) 2026 ChipFlow Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! GF180MCU standard-cell library support.
//!
//! Sibling of [`crate::sky130`]; together with [`crate::gf180mcu_pdk`] this
//! module provides cell detection, pin direction, and library identification
//! for GlobalFoundries' open-source 180 nm MCU PDK across both
//! `gf180mcu_fd_sc_mcu7t5v0` (7-track, 5 V) and `gf180mcu_fd_sc_mcu9t5v0`
//! (9-track, 5 V) standard-cell libraries.
//!
//! Cell-name convention:
//! `gf180mcu_fd_sc_<track>__<celltype>_<drive>`
//! e.g. `gf180mcu_fd_sc_mcu7t5v0__nand2_1`.
//!
//! Implementation phases land per `docs/plans/gf180mcu-enablement.md`.
//! This module is currently a skeleton (Phase 0); detection and pin
//! provider logic land in Phases 1–2.

#![allow(unused)]
