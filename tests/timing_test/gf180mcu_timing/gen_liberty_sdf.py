#!/usr/bin/env python3
"""
Generate Liberty-only SDF from GF180MCU cells used in a design.

Usage: python3 gen_liberty_sdf.py <design.v> [output.sdf]

Mirrors `sky130_timing/gen_liberty_sdf.py` for the GF180MCU
gf180mcu_fd_sc_mcu7t5v0 standard-cell library. Timing values are
representative typical-corner (tt_025C_5v00) numbers extracted from
the Liberty file at minimum input slew / minimum output load.
Pre-layout SDF (no routing parasitics).
"""

import sys
import re
from pathlib import Path
from collections import defaultdict

# GF180MCU 7t5v0 standard cell timing values (in ps) at typ corner
# tt_025C_5v00, minimum slew / minimum load. Sourced from the per-cell
# Liberty tables in vendor/gf180mcu_fd_sc_mcu7t5v0/cells/<cell>/*.lib.
GF180MCU_TIMING = {
    "gf180mcu_fd_sc_mcu7t5v0__inv_1": {
        "setup": 0,
        "hold": 0,
        "clk_to_q": 0,
        # Pin names: I, ZN (note: not A/Y like sky130).
        "iopath": {"I": {"ZN": 38}},  # cell_rise/fall ≈ 38-50ps @ min load
    },
    "gf180mcu_fd_sc_mcu7t5v0__buf_1": {
        "setup": 0,
        "hold": 0,
        "clk_to_q": 0,
        "iopath": {"I": {"Z": 60}},
    },
    "gf180mcu_fd_sc_mcu7t5v0__nand2_1": {
        "setup": 0,
        "hold": 0,
        "clk_to_q": 0,
        # 2-input NAND, output is ZN.
        "iopath": {"A1": {"ZN": 60}, "A2": {"ZN": 60}},
    },
    "gf180mcu_fd_sc_mcu7t5v0__nor2_1": {
        "setup": 0,
        "hold": 0,
        "clk_to_q": 0,
        "iopath": {"A1": {"ZN": 70}, "A2": {"ZN": 70}},
    },
    "gf180mcu_fd_sc_mcu7t5v0__and2_1": {
        "setup": 0,
        "hold": 0,
        "clk_to_q": 0,
        "iopath": {"A1": {"Z": 80}, "A2": {"Z": 80}},
    },
    "gf180mcu_fd_sc_mcu7t5v0__dffq_1": {
        # Setup_rising fall_constraint @ tt_025C_5v00 min-slew ≈ 229ps.
        # Hold_rising fall_constraint @ same operating point ≈ 86ps.
        # Rising-edge clk-to-Q ≈ 320ps from the rising_edge IOPATH.
        "setup": 230,
        "hold": 86,
        "clk_to_q": 320,
        "iopath": {},
    },
}


def parse_verilog(verilog_file):
    """Extract cell instances from Verilog file."""
    instances = defaultdict(list)

    with open(verilog_file) as f:
        content = f.read()

    # Pattern: gf180mcu_fd_sc_mcu{7,9}t5v0__<cellname>_<drive> instance_name (...)
    pattern = r'(\S+?)\s+(\w+)\s*\('

    for match in re.finditer(pattern, content):
        cell_type = match.group(1)
        inst_name = match.group(2)

        if cell_type.startswith('gf180mcu_fd_sc_mcu'):
            instances[cell_type].append(inst_name)

    return instances


def gen_sdf_header(design_name):
    """Generate SDF file header."""
    return f"""(DELAYFILE
  (SDFVERSION "3.0")
  (DESIGN "{design_name}")
  (DATE "{design_name} pre-layout Liberty-only SDF")
  (VENDOR "Jacquard")
  (PROGRAM "gen_liberty_sdf.py")
  (VERSION "1.0")
  (DIVIDER /)
  (VOLTAGE 5.000::5.000)
  (PROCESS "typical")
  (TEMPERATURE 25.000::25.000)
  (TIMESCALE 1ps)

"""


def gen_cell_sdf(cell_type, instance_name, timing):
    """Generate SDF CELL section for one instance."""
    sdf = f"""  (CELL
    (CELLTYPE "{cell_type}")
    (INSTANCE {instance_name})
"""

    # Setup and hold timing (sequential cells). GF180MCU DFFs use the
    # rising-edge CLK convention (the negative-edge dffnq variant uses
    # CLKN, which this script doesn't currently target).
    if timing.get("setup", 0) != 0:
        sdf += f"""    (TIMINGCHECK
      (SETUP D (POSEDGE CLK) ({timing["setup"]} {timing["setup"]}))
    )
"""

    if timing.get("hold", 0) != 0:
        sdf += f"""    (TIMINGCHECK
      (HOLD D (POSEDGE CLK) ({timing["hold"]} {timing["hold"]}))
    )
"""

    # IOPATH delays (combinational paths)
    if timing.get("iopath"):
        sdf += "    (DELAY\n      (ABSOLUTE\n"
        for input_pin, outputs in timing["iopath"].items():
            for output_pin, delay in outputs.items():
                sdf += f"        (IOPATH {input_pin} {output_pin} ({delay} {delay}) ({delay} {delay}))\n"
        sdf += "      )\n    )\n"

    # CLK to Q delay (sequential cells)
    if timing.get("clk_to_q", 0) != 0:
        sdf += f"""    (DELAY
      (ABSOLUTE
        (IOPATH (POSEDGE CLK) Q ({timing["clk_to_q"]} {timing["clk_to_q"]}) ({timing["clk_to_q"]} {timing["clk_to_q"]}))
      )
    )
"""

    sdf += "  )\n\n"
    return sdf


def generate_sdf(verilog_file, output_file=None):
    """Generate SDF from Verilog design file."""
    if output_file is None:
        output_file = str(Path(verilog_file).with_suffix(".sdf"))

    design_name = Path(verilog_file).stem
    instances = parse_verilog(verilog_file)

    if not instances:
        print(f"Warning: No GF180MCU cell instances found in {verilog_file}", file=sys.stderr)
        return None

    sdf_content = gen_sdf_header(design_name)

    # Generate SDF for each instance
    for cell_type in sorted(instances.keys()):
        timing = GF180MCU_TIMING.get(cell_type)
        if timing is None:
            print(f"Warning: Timing not available for {cell_type}", file=sys.stderr)
            continue

        for instance_name in instances[cell_type]:
            sdf_content += gen_cell_sdf(cell_type, instance_name, timing)

    sdf_content += ")\n"

    # Write SDF file
    with open(output_file, 'w') as f:
        f.write(sdf_content)

    print(f"Generated {output_file} with {sum(len(v) for v in instances.values())} cell instances", file=sys.stderr)
    return output_file


if __name__ == "__main__":
    if len(sys.argv) < 2:
        print("Usage: python3 gen_liberty_sdf.py <design.v> [output.sdf]", file=sys.stderr)
        sys.exit(1)

    verilog_file = sys.argv[1]
    output_file = sys.argv[2] if len(sys.argv) > 2 else None

    if not Path(verilog_file).exists():
        print(f"Error: File not found: {verilog_file}", file=sys.stderr)
        sys.exit(1)

    generate_sdf(verilog_file, output_file)
