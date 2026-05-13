//! End-to-end integration test: invokes a real OpenSTA build and parses
//! its dump.
//!
//! Gated on `opensta_to_ir::opensta::find_opensta` returning `Some`. When
//! the OpenSTA binary is unbuilt, the test reports skip and exits clean
//! rather than failing. Build OpenSTA with `scripts/build-opensta.sh`.

use std::path::Path;
use std::process::Command;

use opensta_to_ir::opensta::find_opensta;
use tempfile::TempDir;
use timing_ir::root_as_timing_ir;

const TINY_VERILOG: &str = r#"
module tiny(A, B, Y);
  input A, B;
  output Y;
  AND2_00_0 u1 (.A(A), .B(B), .Y(Y));
endmodule
"#;

const DFF_VERILOG: &str = r#"
module dff_test(CLK, D, Q);
  input CLK, D;
  output Q;
  DFF d1 (.CLK(CLK), .D(D), .Q(Q));
endmodule
"#;

// Two-AND chain — the wire from u1.Y to u2.A is the load-bearing
// interconnect we want SDF back-annotation to populate.
const CHAIN_VERILOG: &str = r#"
module chain(A, B, C, Y);
  input A, B, C;
  output Y;
  wire mid;
  AND2_00_0 u1 (.A(A), .B(B), .Y(mid));
  AND2_00_0 u2 (.A(mid), .B(C), .Y(Y));
endmodule
"#;

// Minimal SDC defining a 10 ns clock on `CLK`. Required for OpenSTA to
// propagate clock arrivals into the timing graph.
const DFF_SDC: &str = "create_clock -name clk -period 10.0 [get_ports CLK]\n";

// Minimal SDF with one INTERCONNECT entry on the wire u1/Y → u2/A.
// Hierarchy separator matches the design's flat namespace.
const CHAIN_SDF: &str = r#"(DELAYFILE
  (SDFVERSION "3.0")
  (DESIGN "chain")
  (DATE "Mon Jan 01 00:00:00 2024")
  (VENDOR "test")
  (PROGRAM "test")
  (VERSION "0.0.0")
  (DIVIDER /)
  (VOLTAGE 1.800::1.800)
  (PROCESS "typical")
  (TEMPERATURE 25.000::25.000)
  (TIMESCALE 1ns)
  (CELL
    (CELLTYPE "chain")
    (INSTANCE)
    (DELAY
      (ABSOLUTE
        (INTERCONNECT u1/Y u2/A (0.040:0.050:0.060) (0.045:0.055:0.065))
      )
    )
  )
)
"#;

fn bin() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_opensta-to-ir"))
}

fn aigpdk_lib() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("aigpdk/aigpdk_nomem.lib")
}

// Mirrors pyproject.toml::[tool.jacquard.pdks.sky130].volare_hash. If
// volare is enabled at a different hash the test skips rather than
// silently exercising the wrong PDK.
const PINNED_SKY130_VOLARE_HASH: &str = "c6d73a35f524070e85faff4a6a9eef49553ebc2b";

/// `$SKY130_LIBERTY_DIR` overrides the volare-default lookup path.
fn find_sky130_lib_dir() -> Option<std::path::PathBuf> {
    if let Ok(env_dir) = std::env::var("SKY130_LIBERTY_DIR") {
        let p = std::path::PathBuf::from(env_dir);
        if p.is_dir() {
            return Some(p);
        }
    }
    let home = std::env::var_os("HOME")?;
    let p = std::path::PathBuf::from(home)
        .join(".volare/volare/sky130/versions")
        .join(PINNED_SKY130_VOLARE_HASH)
        .join("sky130A/libs.ref/sky130_fd_sc_hd/lib");
    p.is_dir().then_some(p)
}

const SKY130_DFF_VERILOG: &str = r#"
module sky130_dff(CLK, D, Q);
  input CLK, D;
  output Q;
  sky130_fd_sc_hd__dfxtp_1 dff (.CLK(CLK), .D(D), .Q(Q));
endmodule
"#;

// Mirrors pyproject.toml::[tool.jacquard.pdks.gf180mcu].volare_hash.
// If volare is enabled at a different hash the test skips rather than
// silently exercising the wrong PDK.
const PINNED_GF180MCU_VOLARE_HASH: &str = "559a117b163cef2f920f33f30f6f690aa0b47e4c";

/// `$GF180MCU_LIBERTY_DIR` overrides the volare-default lookup path.
fn find_gf180mcu_lib_dir() -> Option<std::path::PathBuf> {
    if let Ok(env_dir) = std::env::var("GF180MCU_LIBERTY_DIR") {
        let p = std::path::PathBuf::from(env_dir);
        if p.is_dir() {
            return Some(p);
        }
    }
    let home = std::env::var_os("HOME")?;
    let p = std::path::PathBuf::from(home)
        .join(".volare/volare/gf180mcu/versions")
        .join(PINNED_GF180MCU_VOLARE_HASH)
        .join("gf180mcuC/libs.ref/gf180mcu_fd_sc_mcu7t5v0/liberty");
    p.is_dir().then_some(p)
}

const GF180MCU_DFF_VERILOG: &str = r#"
module gf180mcu_dff(CLK, D, Q);
  input CLK, D;
  output Q;
  gf180mcu_fd_sc_mcu7t5v0__dffq_1 dff (.CLK(CLK), .D(D), .Q(Q), .notifier(1'b0));
endmodule
"#;

#[test]
fn aigpdk_and2_emits_two_arcs() {
    let Some(_sta) = find_opensta(None) else {
        eprintln!("skipping: OpenSTA not built; run scripts/build-opensta.sh");
        return;
    };

    let dir = TempDir::new().unwrap();
    let v_path = dir.path().join("tiny.v");
    let out_path = dir.path().join("tiny.jtir");
    std::fs::write(&v_path, TINY_VERILOG).unwrap();

    let lib = aigpdk_lib();
    assert!(lib.exists(), "AIGPDK Liberty missing at {}", lib.display());

    let output = Command::new(bin())
        .arg("--liberty")
        .arg(&lib)
        .arg("--verilog")
        .arg(&v_path)
        .arg("--top")
        .arg("tiny")
        .arg("--output")
        .arg(&out_path)
        .output()
        .expect("run opensta-to-ir");

    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}\nstdout: {}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout),
    );

    let buf = std::fs::read(&out_path).expect("output IR written");
    let ir = root_as_timing_ir(&buf).expect("readable IR");

    let corners = ir.corners().expect("corners present");
    assert_eq!(corners.len(), 1);
    assert_eq!(corners.get(0).name(), Some("default"));

    // AND2_00_0 has two combinational paths: A→Y and B→Y. AIGPDK Liberty
    // assigns 1 ps to each, so we expect two arcs both at 1 ps rise/fall.
    let arcs = ir.timing_arcs().expect("arcs vector present");
    assert_eq!(arcs.len(), 2, "expected A→Y and B→Y arcs from AND2_00_0");

    let mut driver_pins: Vec<String> = (0..arcs.len())
        .map(|i| arcs.get(i).driver_pin().unwrap_or("").to_string())
        .collect();
    driver_pins.sort();
    assert_eq!(driver_pins, vec!["A".to_string(), "B".to_string()]);

    for i in 0..arcs.len() {
        let arc = arcs.get(i);
        assert_eq!(arc.cell_instance(), Some("u1"));
        assert_eq!(arc.load_pin(), Some("Y"));
        let rise = arc.rise_delay().unwrap();
        let r = rise.get(0);
        // AIGPDK 1ps with float scaling — accept anything up to 5 ps.
        assert!(r.max() < 5.0, "arc {} rise_max {} too large", i, r.max());
        assert!(r.max() > 0.0, "arc {} rise_max should be non-zero", i);
    }
}

#[test]
fn aigpdk_dff_emits_setup_hold_records() {
    let Some(_sta) = find_opensta(None) else {
        eprintln!("skipping: OpenSTA not built; run scripts/build-opensta.sh");
        return;
    };

    let dir = TempDir::new().unwrap();
    let v_path = dir.path().join("dff.v");
    let out_path = dir.path().join("dff.jtir");
    std::fs::write(&v_path, DFF_VERILOG).unwrap();

    let lib = aigpdk_lib();
    let output = Command::new(bin())
        .arg("--liberty")
        .arg(&lib)
        .arg("--verilog")
        .arg(&v_path)
        .arg("--top")
        .arg("dff_test")
        .arg("--output")
        .arg(&out_path)
        .output()
        .expect("run opensta-to-ir");

    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}\nstdout: {}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout),
    );

    let buf = std::fs::read(&out_path).expect("output IR written");
    let ir = root_as_timing_ir(&buf).expect("readable IR");

    // Expect at least one CLK→Q delay arc.
    let arcs = ir.timing_arcs().expect("arcs vector present");
    assert!(!arcs.is_empty(), "expected CLK→Q arc, got 0");

    // Setup/hold checks for the DFF — AIGPDK Liberty defines both rising
    // and falling, so we expect 2 records keyed by edge.
    let checks = ir.setup_hold_checks().expect("setup_hold vector present");
    assert!(
        !checks.is_empty(),
        "expected at least one SETUP_HOLD record, got 0"
    );

    let c0 = checks.get(0);
    assert_eq!(c0.cell_instance(), Some("d1"));
    assert_eq!(c0.d_pin(), Some("D"));
    assert_eq!(c0.clk_pin(), Some("CLK"));
    let setup = c0.setup().expect("setup values");
    assert_eq!(setup.len(), 1, "single corner");
    let hold = c0.hold().expect("hold values");
    assert_eq!(hold.len(), 1, "single corner");
}

#[test]
fn chain_with_sdf_emits_interconnect_delay() {
    let Some(_sta) = find_opensta(None) else {
        eprintln!("skipping: OpenSTA not built; run scripts/build-opensta.sh");
        return;
    };

    let dir = TempDir::new().unwrap();
    let v_path = dir.path().join("chain.v");
    let sdf_path = dir.path().join("chain.sdf");
    let out_path = dir.path().join("chain.jtir");
    std::fs::write(&v_path, CHAIN_VERILOG).unwrap();
    std::fs::write(&sdf_path, CHAIN_SDF).unwrap();

    let lib = aigpdk_lib();
    let output = Command::new(bin())
        .arg("--liberty")
        .arg(&lib)
        .arg("--verilog")
        .arg(&v_path)
        .arg("--sdf")
        .arg(&sdf_path)
        .arg("--top")
        .arg("chain")
        .arg("--output")
        .arg(&out_path)
        .output()
        .expect("run opensta-to-ir");

    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}\nstdout: {}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout),
    );

    let buf = std::fs::read(&out_path).expect("output IR written");
    let ir = root_as_timing_ir(&buf).expect("readable IR");

    let ics = ir
        .interconnect_delays()
        .expect("interconnect_delays vector present");
    assert!(
        !ics.is_empty(),
        "expected at least one INTERCONNECT (annotated wire u1/Y → u2/A); got 0"
    );

    // Find the u1/Y → u2/A interconnect.
    let mut found = None;
    for i in 0..ics.len() {
        let ic = ics.get(i);
        if ic.from_pin() == Some("u1/Y") && ic.to_pin() == Some("u2/A") {
            found = Some(ic);
            break;
        }
    }
    let ic = found.expect("u1/Y → u2/A interconnect missing from IR");
    let delay = ic.delay().expect("delay vector");
    assert_eq!(delay.len(), 1, "single corner");
    let v = delay.get(0);
    assert_eq!(v.corner_index(), 0);
    // SDF entry was 0.040..0.060 ns → 40..60 ps. Allow either rise or fall
    // group (model takes max across them = 65 ps).
    assert!(
        v.max() >= 40.0 && v.max() <= 70.0,
        "interconnect max {} ps outside expected 40..70 ps from SDF",
        v.max()
    );
}

#[test]
fn dff_with_sdc_clock_emits_clock_arrival() {
    let Some(_sta) = find_opensta(None) else {
        eprintln!("skipping: OpenSTA not built; run scripts/build-opensta.sh");
        return;
    };

    let dir = TempDir::new().unwrap();
    let v_path = dir.path().join("dff.v");
    let sdc_path = dir.path().join("dff.sdc");
    let out_path = dir.path().join("dff.jtir");
    std::fs::write(&v_path, DFF_VERILOG).unwrap();
    std::fs::write(&sdc_path, DFF_SDC).unwrap();

    let lib = aigpdk_lib();
    let output = Command::new(bin())
        .arg("--liberty")
        .arg(&lib)
        .arg("--verilog")
        .arg(&v_path)
        .arg("--sdc")
        .arg(&sdc_path)
        .arg("--top")
        .arg("dff_test")
        .arg("--output")
        .arg(&out_path)
        .output()
        .expect("run opensta-to-ir");

    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}\nstdout: {}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout),
    );

    let buf = std::fs::read(&out_path).expect("output IR written");
    let ir = root_as_timing_ir(&buf).expect("readable IR");

    let cas = ir.clock_arrivals().expect("clock_arrivals vector present");
    assert!(
        !cas.is_empty(),
        "expected at least one CLOCK_ARRIVAL for d1/CLK; got 0"
    );

    let mut found = None;
    for i in 0..cas.len() {
        let ca = cas.get(i);
        if ca.cell_instance() == Some("d1") && ca.clk_pin() == Some("CLK") {
            found = Some(ca);
            break;
        }
    }
    let ca = found.expect("d1/CLK clock arrival missing from IR");
    let arr = ca.arrival().expect("arrival values present");
    assert_eq!(arr.len(), 1, "single corner");
    let v = arr.get(0);
    assert_eq!(v.corner_index(), 0);
    // For a single-DFF design with the clock applied directly to a top-level
    // port, propagated clock arrival is small but nonzero (clock pin
    // capacitive load adds ~ps). Max should be finite and non-negative;
    // exact value depends on Liberty/parasitics, so don't pin it.
    assert!(
        v.max().is_finite() && v.max() >= 0.0 && v.max() < 100_000.0,
        "clock arrival max {} ps outside sane finite range",
        v.max()
    );
}

/// WS2.4 multi-corner pipeline test. Loads the same AIGPDK Liberty
/// under two named corners (`typ`, `slow`) and verifies the IR ends
/// up with two corner records and two TimingValue entries per timing
/// record. Values are identical (same Liberty), so this is a
/// structural check; per-value differences land when sky130 multi-
/// corner Liberty is wired in.
#[test]
fn aigpdk_dff_emits_per_corner_timing_values() {
    let Some(_sta) = find_opensta(None) else {
        eprintln!("skipping: OpenSTA not built; run scripts/build-opensta.sh");
        return;
    };

    let dir = TempDir::new().unwrap();
    let v_path = dir.path().join("dff.v");
    let out_path = dir.path().join("dff.jtir");
    std::fs::write(&v_path, DFF_VERILOG).unwrap();

    let lib = aigpdk_lib();
    assert!(lib.exists(), "AIGPDK Liberty missing at {}", lib.display());

    let output = Command::new(bin())
        .arg("--liberty")
        .arg(format!("typ={}", lib.display()))
        .arg("--liberty")
        .arg(format!("slow={}", lib.display()))
        .arg("--verilog")
        .arg(&v_path)
        .arg("--top")
        .arg("dff_test")
        .arg("--output")
        .arg(&out_path)
        .output()
        .expect("run opensta-to-ir");

    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}\nstdout: {}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout),
    );

    let buf = std::fs::read(&out_path).expect("output IR written");
    let ir = root_as_timing_ir(&buf).expect("readable IR");

    let corners = ir.corners().expect("corners present");
    assert_eq!(corners.len(), 2, "expected typ + slow corners");
    assert_eq!(corners.get(0).name(), Some("typ"));
    assert_eq!(corners.get(1).name(), Some("slow"));

    // Each setup/hold record should carry one TimingValue per corner.
    let checks = ir.setup_hold_checks().expect("checks present");
    assert!(checks.len() > 0, "expected at least one DFF setup/hold");
    for i in 0..checks.len() {
        let check = checks.get(i);
        let setup = check.setup().expect("setup vector present");
        let hold = check.hold().expect("hold vector present");
        assert_eq!(setup.len(), 2, "two corners → two setup TimingValues");
        assert_eq!(hold.len(), 2, "two corners → two hold TimingValues");
        // Corner indices are positional; entry 0 → corner 0, etc.
        let setup_indices: Vec<u32> = (0..setup.len()).map(|j| setup.get(j).corner_index()).collect();
        let hold_indices: Vec<u32> = (0..hold.len()).map(|j| hold.get(j).corner_index()).collect();
        assert_eq!(setup_indices, vec![0, 1]);
        assert_eq!(hold_indices, vec![0, 1]);
    }
}

/// Multi-corner with real sky130 Liberty. The companion to
/// `aigpdk_dff_emits_per_corner_timing_values`: same shape, but loads
/// three genuine PVT corners (typ / slow / fast) and asserts the
/// resulting setup TimingValues actually differ across corners. Skips
/// when sky130 isn't installed locally.
#[test]
fn sky130_multi_corner_emits_per_corner_values() {
    let Some(_sta) = find_opensta(None) else {
        eprintln!("skipping: OpenSTA not built; run scripts/build-opensta.sh");
        return;
    };

    let Some(lib_dir) = find_sky130_lib_dir() else {
        eprintln!(
            "skipping: sky130 PDK not installed at expected volare path. \
             Install with `uv sync --group dev && uv run volare enable {}`, \
             or set $SKY130_LIBERTY_DIR to a directory containing \
             sky130_fd_sc_hd__{{tt,ss,ff}}_*.lib files.",
            &PINNED_SKY130_VOLARE_HASH[..8],
        );
        return;
    };

    let typ_lib = lib_dir.join("sky130_fd_sc_hd__tt_025C_1v80.lib");
    let slow_lib = lib_dir.join("sky130_fd_sc_hd__ss_n40C_1v60.lib");
    let fast_lib = lib_dir.join("sky130_fd_sc_hd__ff_n40C_1v95.lib");
    for l in [&typ_lib, &slow_lib, &fast_lib] {
        assert!(l.exists(), "expected Liberty at {}", l.display());
    }

    let dir = TempDir::new().unwrap();
    let v_path = dir.path().join("sky130_dff.v");
    let out_path = dir.path().join("sky130_dff.jtir");
    std::fs::write(&v_path, SKY130_DFF_VERILOG).unwrap();

    let output = Command::new(bin())
        .arg("--liberty")
        .arg(format!("typ={}", typ_lib.display()))
        .arg("--liberty")
        .arg(format!("slow={}", slow_lib.display()))
        .arg("--liberty")
        .arg(format!("fast={}", fast_lib.display()))
        .arg("--verilog")
        .arg(&v_path)
        .arg("--top")
        .arg("sky130_dff")
        .arg("--output")
        .arg(&out_path)
        .output()
        .expect("run opensta-to-ir");

    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}\nstdout: {}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout),
    );

    let buf = std::fs::read(&out_path).expect("output IR written");
    let ir = root_as_timing_ir(&buf).expect("readable IR");

    let corners = ir.corners().expect("corners present");
    assert_eq!(corners.len(), 3, "expected typ + slow + fast corners");
    assert_eq!(corners.get(0).name(), Some("typ"));
    assert_eq!(corners.get(1).name(), Some("slow"));
    assert_eq!(corners.get(2).name(), Some("fast"));

    let checks = ir.setup_hold_checks().expect("checks present");
    assert!(checks.len() > 0, "expected at least one DFF setup/hold");

    for i in 0..checks.len() {
        let check = checks.get(i);
        let setup = check.setup().expect("setup vector present");
        let hold = check.hold().expect("hold vector present");
        assert_eq!(setup.len(), 3, "three corners → three setup TimingValues");
        assert_eq!(hold.len(), 3, "three corners → three hold TimingValues");

        assert_eq!(setup.get(0).corner_index(), 0);
        assert_eq!(setup.get(1).corner_index(), 1);
        assert_eq!(setup.get(2).corner_index(), 2);

        let setup_typ = setup.get(0).max();
        let setup_slow = setup.get(1).max();
        let setup_fast = setup.get(2).max();

        // Regression sentinel: dfxtp_1 has non-zero setup at every sky130 corner.
        assert!(
            setup_typ.abs() > 1e-9,
            "setup at typ corner is zero — Liberty parsing regression? record {i} max={setup_typ}",
        );

        // Per-corner setup ordering — the load-bearing claim this test exists to prove.
        assert!(
            setup_slow > setup_fast,
            "slow setup ({setup_slow} ps) should exceed fast setup ({setup_fast} ps) on record {i}",
        );
        assert!(
            setup_slow >= setup_typ,
            "slow setup ({setup_slow} ps) should be >= typ ({setup_typ} ps) on record {i}",
        );
        assert!(
            setup_typ >= setup_fast,
            "typ setup ({setup_typ} ps) should be >= fast ({setup_fast} ps) on record {i}",
        );
    }
}

/// Multi-corner with real GF180MCU 7t5v0 Liberty. The companion to
/// `sky130_multi_corner_emits_per_corner_values`: same shape, but loads
/// three GF180MCU PVT corners (typ / slow / fast) at the 5.0 V
/// nominal operating point and asserts the resulting setup TimingValues
/// actually differ across corners. Skips when the GF180MCU PDK isn't
/// installed locally.
///
/// Corner choices mirror the SKY130 test's slow/typ/fast ordering, but
/// reach into GlobalFoundries 180 nm naming:
///   typ  = tt_025C_5v00   (typical-typical, 25 C, 5.0 V)
///   slow = ss_125C_4v50   (slow-slow,    125 C, 4.5 V)
///   fast = ff_n40C_5v50   (fast-fast,    -40 C, 5.5 V)
#[test]
fn gf180mcu_multi_corner_emits_per_corner_values() {
    let Some(_sta) = find_opensta(None) else {
        eprintln!("skipping: OpenSTA not built; run scripts/build-opensta.sh");
        return;
    };

    let Some(lib_dir) = find_gf180mcu_lib_dir() else {
        eprintln!(
            "skipping: gf180mcu PDK not installed at expected volare path. \
             Install with `uv sync --group dev && uv run volare enable --pdk gf180mcu {}`, \
             or set $GF180MCU_LIBERTY_DIR to a directory containing \
             gf180mcu_fd_sc_mcu7t5v0__{{tt,ss,ff}}_*.lib files.",
            &PINNED_GF180MCU_VOLARE_HASH[..8],
        );
        return;
    };

    let typ_lib = lib_dir.join("gf180mcu_fd_sc_mcu7t5v0__tt_025C_5v00.lib");
    let slow_lib = lib_dir.join("gf180mcu_fd_sc_mcu7t5v0__ss_125C_4v50.lib");
    let fast_lib = lib_dir.join("gf180mcu_fd_sc_mcu7t5v0__ff_n40C_5v50.lib");
    for l in [&typ_lib, &slow_lib, &fast_lib] {
        assert!(l.exists(), "expected Liberty at {}", l.display());
    }

    let dir = TempDir::new().unwrap();
    let v_path = dir.path().join("gf180mcu_dff.v");
    let out_path = dir.path().join("gf180mcu_dff.jtir");
    std::fs::write(&v_path, GF180MCU_DFF_VERILOG).unwrap();

    let output = Command::new(bin())
        .arg("--liberty")
        .arg(format!("typ={}", typ_lib.display()))
        .arg("--liberty")
        .arg(format!("slow={}", slow_lib.display()))
        .arg("--liberty")
        .arg(format!("fast={}", fast_lib.display()))
        .arg("--verilog")
        .arg(&v_path)
        .arg("--top")
        .arg("gf180mcu_dff")
        .arg("--output")
        .arg(&out_path)
        .output()
        .expect("run opensta-to-ir");

    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}\nstdout: {}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout),
    );

    let buf = std::fs::read(&out_path).expect("output IR written");
    let ir = root_as_timing_ir(&buf).expect("readable IR");

    let corners = ir.corners().expect("corners present");
    assert_eq!(corners.len(), 3, "expected typ + slow + fast corners");
    assert_eq!(corners.get(0).name(), Some("typ"));
    assert_eq!(corners.get(1).name(), Some("slow"));
    assert_eq!(corners.get(2).name(), Some("fast"));

    let checks = ir.setup_hold_checks().expect("checks present");
    assert!(checks.len() > 0, "expected at least one DFF setup/hold");

    for i in 0..checks.len() {
        let check = checks.get(i);
        let setup = check.setup().expect("setup vector present");
        let hold = check.hold().expect("hold vector present");
        assert_eq!(setup.len(), 3, "three corners → three setup TimingValues");
        assert_eq!(hold.len(), 3, "three corners → three hold TimingValues");

        assert_eq!(setup.get(0).corner_index(), 0);
        assert_eq!(setup.get(1).corner_index(), 1);
        assert_eq!(setup.get(2).corner_index(), 2);

        let setup_typ = setup.get(0).max();
        let setup_slow = setup.get(1).max();
        let setup_fast = setup.get(2).max();

        // Regression sentinel: dffq_1 has non-zero setup at every GF180MCU corner.
        assert!(
            setup_typ.abs() > 1e-9,
            "setup at typ corner is zero — Liberty parsing regression? record {i} max={setup_typ}",
        );

        // Per-corner setup ordering — the load-bearing claim this test exists to prove.
        // Slow PVT pessimism widens the setup window relative to typ; fast PVT shrinks it.
        assert!(
            setup_slow > setup_fast,
            "slow setup ({setup_slow} ps) should exceed fast setup ({setup_fast} ps) on record {i}",
        );
        assert!(
            setup_slow >= setup_typ,
            "slow setup ({setup_slow} ps) should be >= typ ({setup_typ} ps) on record {i}",
        );
        assert!(
            setup_typ >= setup_fast,
            "typ setup ({setup_typ} ps) should be >= fast ({setup_fast} ps) on record {i}",
        );
    }
}
