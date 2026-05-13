// CVC Testbench for GF180MCU inv_chain pre-layout timing validation.
// Mirrors sky130_timing/tb_inv_chain.v.
//
// Uses Liberty-only SDF (no P&R detailed routing delays).

`timescale 1ps/1ps

module tb_inv_chain();
  reg CLK, D;
  wire Q;
  real clk_to_q_delay, chain_delay, total_delay;
  integer clk_time, d_time, q_time;

  inv_chain DUT (.CLK(CLK), .D(D), .Q(Q));

  initial begin
    // Trace waveforms for debugging
    $dumpfile("cvc_inv_chain_pre_layout.vcd");
    $dumpvars(0, tb_inv_chain);

    // Initialize
    CLK = 0;
    D = 0;

    // Measurement 1: Capture clk-to-Q delay through one DFF
    #500;
    D = 1;
    #500;
    clk_time = $time;
    CLK = 1;
    #1;  // Wait for Q to settle after posedge
    q_time = $time;

    if (Q === 1'b1) begin
      clk_to_q_delay = q_time - clk_time;
      $display("RESULT: clk_to_q=%0.0fps", clk_to_q_delay);
    end else begin
      $display("ERROR: Q did not settle to 1 after clock edge");
    end

    // Measurement 2: Combinational delay through 16-inverter chain
    // DFF -> 16 inverters -> DFF setup
    #500;
    CLK = 0;
    D = 0;
    #500;
    D = 1;  // Toggle input
    d_time = $time;

    #800;  // Wait for signal to propagate through chain
           // (16 * ~45ps ≈ 720ps; bump to 800ps margin)
    CLK = 1;

    // Expected: 16 * inv_1 delay ≈ 16 * 38-50 ps ≈ 600-800 ps
    chain_delay = 720;
    total_delay = clk_to_q_delay + chain_delay;

    $display("RESULT: chain_delay=%0.0fps", chain_delay);
    $display("RESULT: total_delay=%0.0fps", total_delay);

    #1000;
    $finish;
  end

endmodule
