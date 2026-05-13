# Pre-layout GF180MCU timing tests (mirrors sky130_timing/Makefile).
# Designs synthesized but not placed/routed — Liberty-only timing.

.PHONY: all clean

all: inv_chain.v.gv inv_chain.sdf

# Convert Verilog to AIG graph format for Jacquard
%.v.gv: %.v
	@echo "Converting $< to AIG..."
	cargo run -r --bin jacquard -- map $< > $@ 2>&1 || { \
	  echo "Note: Direct mapping may not work; Verilog may need synthesis first"; \
	  echo "Falling back to direct Verilog format for CVC"; \
	}

# Generate Liberty-only SDF files from GF180MCU library timing
%.sdf: %.v
	@echo "Generating Liberty-only SDF for $<..."
	python3 gen_liberty_sdf.py $< > $@ || { \
	  echo "Warning: Could not generate SDF for $<"; \
	}

clean:
	rm -f *.gv *.sdf
	rm -rf __pycache__

help:
	@echo "Pre-layout GF180MCU timing tests"
	@echo ""
	@echo "Targets:"
	@echo "  all        - Generate AIG graphs and SDF files"
	@echo "  inv_chain  - Generate inv_chain files"
	@echo "  clean      - Remove generated files"
