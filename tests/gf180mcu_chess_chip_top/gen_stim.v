// Stim generator for the wafer.space chess chip_top netlist.
//
// Drives clk_PAD, rst_n_PAD, input_PAD, bidir_PAD, analog_PAD with a
// minimal smoke pattern:
//   * rst_n_PAD low for 5 clock cycles, then released
//   * clk_PAD toggling at a steady rate for 100 cycles total
//   * all other inputs held at 0
//
// The module name is `chip_top` (not `chip_top_tb`) so Jacquard's
// VCD scope auto-detection matches the gate-level top module without
// requiring --input-vcd-scope.
//
// Build + run:
//   iverilog -o stim_gen gen_stim.v && ./stim_gen
//
// The chess core won't do anything useful without proper JTAG stim
// through bidir_PAD — this exercises the partitioner, sequential
// decomposition, and clock-tree handling at scale, not functional
// behaviour. See README.md.

`timescale 1ns / 1ps

module chip_top;
    reg         clk_PAD;
    reg         rst_n_PAD;
    reg [11:0]  input_PAD;
    reg [39:0]  bidir_PAD;
    reg [1:0]   analog_PAD;

    initial begin
        $dumpfile("stim.vcd");
        $dumpvars(0, chip_top);

        clk_PAD    = 1'b0;
        rst_n_PAD  = 1'b0;
        input_PAD  = 12'h000;
        bidir_PAD  = 40'h0;
        analog_PAD = 2'h0;

        // Hold reset for 5 cycles (10 half-periods of 5ns each = 50ns).
        repeat (10) begin
            #5 clk_PAD = ~clk_PAD;
        end
        rst_n_PAD = 1'b1;

        // Run for 100 more clock cycles.
        repeat (200) begin
            #5 clk_PAD = ~clk_PAD;
        end

        $finish;
    end
endmodule
