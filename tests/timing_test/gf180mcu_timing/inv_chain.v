/* GF180MCU Timing Test 1: Inverter Chain (mirrors sky130_timing/inv_chain.v)
 *
 * dffq_1 -> 16 x inv_1 -> dffq_1
 *
 * Cells used:
 *   gf180mcu_fd_sc_mcu7t5v0__dffq_1   pins: CLK, D, Q, notifier
 *   gf180mcu_fd_sc_mcu7t5v0__inv_1    pins: I, ZN
 *
 * Expected combo delay (Liberty typ corner @ tt_025C_5v00):
 *   inv_1 cell_rise/fall @ min load ≈ 38-50 ps
 *   ⇒ 16 x ~45 ps ≈ 720 ps combinational
 *
 * Expected DFF behaviour:
 *   dffq_1 clk_to_q ≈ rising_edge IOPATH from CLK to Q
 *   setup_rising  ≈ 230 ps @ minimum input slew / load
 *   hold_rising   ≈ 86 ps @ minimum input slew / load
 *
 * Purpose: simplest possible GF180MCU design that exercises one
 * sequential boundary + one combinational path. Mirrors the
 * SKY130 inv_chain.v shape so the test infrastructure is symmetric.
 */

module inv_chain(CLK, D, Q);
  input CLK;
  wire CLK;
  input D;
  wire D;
  output Q;
  wire Q;
  wire q1;
  wire [15:0] c;
  wire notify_in, notify_out;

  gf180mcu_fd_sc_mcu7t5v0__dffq_1 dff_in (
    .CLK(CLK),
    .D(D),
    .Q(q1),
    .notifier(notify_in)
  );

  gf180mcu_fd_sc_mcu7t5v0__inv_1 i0  (.I(q1),    .ZN(c[0]));
  gf180mcu_fd_sc_mcu7t5v0__inv_1 i1  (.I(c[0]),  .ZN(c[1]));
  gf180mcu_fd_sc_mcu7t5v0__inv_1 i2  (.I(c[1]),  .ZN(c[2]));
  gf180mcu_fd_sc_mcu7t5v0__inv_1 i3  (.I(c[2]),  .ZN(c[3]));
  gf180mcu_fd_sc_mcu7t5v0__inv_1 i4  (.I(c[3]),  .ZN(c[4]));
  gf180mcu_fd_sc_mcu7t5v0__inv_1 i5  (.I(c[4]),  .ZN(c[5]));
  gf180mcu_fd_sc_mcu7t5v0__inv_1 i6  (.I(c[5]),  .ZN(c[6]));
  gf180mcu_fd_sc_mcu7t5v0__inv_1 i7  (.I(c[6]),  .ZN(c[7]));
  gf180mcu_fd_sc_mcu7t5v0__inv_1 i8  (.I(c[7]),  .ZN(c[8]));
  gf180mcu_fd_sc_mcu7t5v0__inv_1 i9  (.I(c[8]),  .ZN(c[9]));
  gf180mcu_fd_sc_mcu7t5v0__inv_1 i10 (.I(c[9]),  .ZN(c[10]));
  gf180mcu_fd_sc_mcu7t5v0__inv_1 i11 (.I(c[10]), .ZN(c[11]));
  gf180mcu_fd_sc_mcu7t5v0__inv_1 i12 (.I(c[11]), .ZN(c[12]));
  gf180mcu_fd_sc_mcu7t5v0__inv_1 i13 (.I(c[12]), .ZN(c[13]));
  gf180mcu_fd_sc_mcu7t5v0__inv_1 i14 (.I(c[13]), .ZN(c[14]));
  gf180mcu_fd_sc_mcu7t5v0__inv_1 i15 (.I(c[14]), .ZN(c[15]));

  gf180mcu_fd_sc_mcu7t5v0__dffq_1 dff_out (
    .CLK(CLK),
    .D(c[15]),
    .Q(Q),
    .notifier(notify_out)
  );

endmodule
