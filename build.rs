//! this build script compiles GEM kernels
// SPDX-FileCopyrightText: Copyright (c) 2024 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=csrc");

    generate_gf180mcu_pin_table();

    // Build the C++ SPI flash model
    cc::Build::new()
        .cpp(true)
        .file("csrc/spiflash_model.cc")
        .include("csrc")
        .compile("spiflash_model");

    #[cfg(feature = "cuda")]
    {
        println!("Building CUDA source files for GEM...");
        let csrc_headers = ucc::import_csrc();
        let mut cl_cuda = ucc::cl_cuda();
        cl_cuda.ccbin(false);
        cl_cuda.flag("-lineinfo");
        cl_cuda.flag("-maxrregcount=128");
        cl_cuda
            .debug(false)
            .opt_level(3)
            .include(&csrc_headers)
            .files(["csrc/kernel_v1.cu"]);
        cl_cuda.compile("gemcu");
        println!("cargo:rustc-link-lib=static=gemcu");
        println!("cargo:rustc-link-lib=dylib=cudart");
        ucc::bindgen(["csrc/kernel_v1.cu"], "kernel_v1.rs");
        ucc::export_csrc();
        ucc::make_compile_commands(&[&cl_cuda]);
    }

    #[cfg(feature = "hip")]
    {
        println!("Building HIP source files for GEM...");
        let csrc_headers = ucc::import_csrc();
        let mut cl_hip = ucc::cl_hip();
        cl_hip
            .debug(false)
            .opt_level(3)
            .include(&csrc_headers)
            .file("csrc/kernel_v1.hip.cpp");
        cl_hip.compile("gemhip");
        println!("cargo:rustc-link-lib=static=gemhip");
        // On AMD backend, link amdhip64; on NVIDIA backend, link cudart.
        // The kernel_v1.hip.cpp wrapper handles both via hipcc compilation.
        if std::env::var("HIP_PLATFORM").as_deref() == Ok("nvidia") {
            println!("cargo:rustc-link-lib=dylib=cudart");
            println!("cargo:rustc-link-lib=dylib=cuda");
            let cuda_path =
                std::env::var("CUDA_PATH").unwrap_or_else(|_| "/usr/local/cuda".to_string());
            println!("cargo:rustc-link-search=native={}/lib64", cuda_path);
            println!("cargo:rustc-link-search=native={}/lib", cuda_path);
            println!("cargo:rustc-link-search=native={}/lib64/stubs", cuda_path);
            println!("cargo:rustc-link-search=native={}/lib/stubs", cuda_path);
        } else {
            println!("cargo:rustc-link-lib=dylib=amdhip64");
            let rocm_path = std::env::var("ROCM_PATH").unwrap_or_else(|_| "/opt/rocm".to_string());
            println!("cargo:rustc-link-search=native={}/lib", rocm_path);
        }
        println!("cargo:rerun-if-env-changed=HIP_PLATFORM");
        println!("cargo:rerun-if-env-changed=CUDA_PATH");
        ucc::bindgen(["csrc/kernel_v1.hip.cpp"], "kernel_v1_hip.rs");
        ucc::export_csrc();
        ucc::make_compile_commands(&[&cl_hip]);
    }

    #[cfg(feature = "metal")]
    {
        println!("Building Metal shader for GEM...");
        // Compile Metal shader to metallib
        ucc::cl_metal()
            .file("csrc/kernel_v1.metal")
            .std_version("metal3.0")
            .macos_version_min("14.0")
            .compile("gem_metal");
        // METALLIB_PATH environment variable is set by the compile step
    }
}

// -----------------------------------------------------------------------------
// GF180MCU pin-direction table generator
// -----------------------------------------------------------------------------
//
// Scans the vendored gf180mcu_fd_sc_mcu{7,9}t5v0 cell-model submodules and
// extracts each cell's port directions from its `.functional.v` model.
// Writes `$OUT_DIR/gf180mcu_pins.rs` for `include!` by src/gf180mcu.rs.
//
// 7t and 9t have identical port layouts per cell type, so the table is
// keyed by base cell type (the `extract_cell_type` result). Drive
// strengths within a cell type also share port layouts; the generator
// dedupes them and panics on disagreement (a real Liberty regression).

fn generate_gf180mcu_pin_table() {
    println!("cargo:rerun-if-changed=vendor/gf180mcu_fd_sc_mcu7t5v0/cells");
    println!("cargo:rerun-if-changed=vendor/gf180mcu_fd_sc_mcu9t5v0/cells");

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set");
    let out_path = Path::new(&out_dir).join("gf180mcu_pins.rs");

    let mut table: BTreeMap<String, (Vec<String>, Vec<String>)> = BTreeMap::new();
    for variant_root in [
        "vendor/gf180mcu_fd_sc_mcu7t5v0/cells",
        "vendor/gf180mcu_fd_sc_mcu9t5v0/cells",
    ] {
        let root = Path::new(variant_root);
        if !root.is_dir() {
            // Submodule uninitialised — leave the table empty and let the
            // consumer's runtime panic surface a clear error. CI / docs
            // already document `git submodule update --init`.
            continue;
        }
        for cell_dir in std::fs::read_dir(root).expect("read cells dir").flatten() {
            let path = cell_dir.path();
            if !path.is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(&path).expect("read cell variant dir").flatten() {
                let f = entry.path();
                let Some(name) = f.file_name().and_then(|s| s.to_str()) else {
                    continue;
                };
                // .functional.v is the authoritative port-list source; skip
                // the preprocessed (.pp.v) variant to avoid duplicate work.
                if !name.ends_with(".functional.v") || name.ends_with(".functional.pp.v") {
                    continue;
                }
                let body = std::fs::read_to_string(&f).expect("read functional.v");
                let Some((macro_name, inputs, outputs)) = parse_module_ports(&body) else {
                    continue;
                };
                let cell_type = base_cell_type(&macro_name).to_string();
                match table.get(&cell_type) {
                    None => {
                        table.insert(cell_type, (inputs, outputs));
                    }
                    Some((existing_i, existing_o)) => {
                        // Drive variants and the 7t/9t pair must agree —
                        // otherwise the assumption "keyed by base type"
                        // is broken and we want to know loudly.
                        assert_eq!(
                            existing_i, &inputs,
                            "input pin mismatch for cell type {cell_type}",
                        );
                        assert_eq!(
                            existing_o, &outputs,
                            "output pin mismatch for cell type {cell_type}",
                        );
                    }
                }
            }
        }
    }

    let mut out = String::new();
    out.push_str("// Generated by build.rs from vendor/gf180mcu_fd_sc_mcu{7,9}t5v0 — do not edit.\n\n");
    out.push_str(
        "pub(crate) static GF180MCU_PIN_TABLE: &[(&str, &[(&str, Direction)])] = &[\n",
    );
    for (cell_type, (inputs, outputs)) in &table {
        out.push_str(&format!("    (\"{cell_type}\", &[\n"));
        for pin in inputs {
            out.push_str(&format!("        (\"{pin}\", Direction::I),\n"));
        }
        for pin in outputs {
            out.push_str(&format!("        (\"{pin}\", Direction::O),\n"));
        }
        out.push_str("    ]),\n");
    }
    out.push_str("];\n");

    std::fs::write(&out_path, out).expect("write gf180mcu_pins.rs");
}

/// Parse one `.functional.v` cell model. Returns (module_name, inputs, outputs).
///
/// Grammar is fixed across the gf180mcu PDK: a single `module <name>( ... );`
/// followed by `input <comma-separated>;` and `output <comma-separated>;`
/// lines. No need for a general Verilog parser.
fn parse_module_ports(body: &str) -> Option<(String, Vec<String>, Vec<String>)> {
    let mut module_name: Option<String> = None;
    let mut inputs = Vec::new();
    let mut outputs = Vec::new();

    for line in body.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("module ") {
            if let Some(paren) = rest.find('(') {
                module_name = Some(rest[..paren].trim().to_string());
            }
        } else if let Some(rest) = line.strip_prefix("input ") {
            push_port_names(rest, &mut inputs);
        } else if let Some(rest) = line.strip_prefix("output ") {
            push_port_names(rest, &mut outputs);
        }
    }

    module_name.map(|n| (n, inputs, outputs))
}

fn push_port_names(decl: &str, out: &mut Vec<String>) {
    let cleaned = decl.trim_end_matches(';').trim();
    for name in cleaned.split(',') {
        let name = name.trim();
        if !name.is_empty() {
            out.push(name.to_string());
        }
    }
}

/// Drop the gf180mcu_fd_sc_mcu{7,9}t5v0__ prefix and a trailing numeric
/// drive-strength suffix to recover the base cell type. Mirrors
/// `src/gf180mcu.rs::extract_cell_type` — kept in sync by the
/// assert_eq cross-check in the per-cell-type dedup pass above (drive
/// variants must produce the same key).
fn base_cell_type(macro_name: &str) -> &str {
    let stripped = macro_name
        .strip_prefix("gf180mcu_fd_sc_mcu7t5v0__")
        .or_else(|| macro_name.strip_prefix("gf180mcu_fd_sc_mcu9t5v0__"))
        .unwrap_or(macro_name);
    if let Some(idx) = stripped.rfind('_') {
        let suffix = &stripped[idx + 1..];
        if !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()) {
            return &stripped[..idx];
        }
    }
    stripped
}
