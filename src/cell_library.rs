//! Runtime cell-library loading for third-party IP and user-supplied
//! cells outside Jacquard's vendored PDKs.
//!
//! See [ADR 0010](../../docs/adr/0010-declarative-cell-metadata.md)
//! and `docs/plans/declarative-cell-metadata.md`. This module is the
//! Tier 1 + minimal Tier 2 slice:
//!
//! - `RuntimeCellLibrary` parses one or more user-supplied
//!   `.v` files via `sverilogparse` to populate a runtime
//!   pin-direction table (Tier 1).
//! - The same struct can attach metadata loaded from
//!   `<file>.cells.toml` siblings or an explicit
//!   `--cell-manifest <PATH>.toml` (Tier 2, kind discriminator only
//!   in v1.0).
//! - `ChainedPinProvider` wraps a built-in `LeafPinProvider`
//!   (e.g. `SKY130LeafPins`, `GF180MCULeafPins`) with the runtime
//!   library. Cells the runtime knows about win; cells it does not
//!   delegate to the built-in fallback. Existing tests stay green
//!   because cells not present in the runtime library go through
//!   exactly the same code path as before.
//!
//! Port-mapping schema (`[cells.NAME.ports]` sub-tables) is
//! deliberately deferred to a future ADR — see ADR 0010 § "Deferred
//! to a future ADR".

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use compact_str::CompactString;
use netlistdb::{Direction, LeafPinProvider};
use serde::Deserialize;
use sverilogparse::{SVerilog, SVerilogRange, WireDefType};

/// Per-cell kind discriminator. Mirrors today's hand-curated
/// `is_<pdk>_*` classification lists in `sky130_pdk.rs` /
/// `gf180mcu_pdk.rs`; future schema versions may attach port-mapping
/// metadata alongside.
///
/// Wire format: deserialised from TOML as a lowercase string
/// (`"ram"`, `"filler"`, …). Unknown values fail the manifest
/// parse rather than degrading silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CellKind {
    /// Standard combinational logic cell. Default-routed through
    /// `pdk_decomp::decompose_with_pdk`; manifests will rarely need
    /// to declare this explicitly.
    Std,
    /// Edge-triggered flip-flop.
    Dff,
    /// Level-sensitive latch.
    Latch,
    /// Integrated clock-gating cell.
    ClockGate,
    /// Synchronous RAM macro. **v1.0 semantics: opaque mode —
    /// `RAMBlock` allocated, outputs routed to X-source slots, no
    /// port resolution.** Sufficient for designs whose CPU does not
    /// read SRAM contents at the timescales Jacquard simulates.
    /// Real memory modelling waits on the port-mapping schema.
    Ram,
    /// Physical-only filler / decap. Contributes no logic.
    Filler,
    /// Row-end / boundary cap. Physical-only.
    Endcap,
    /// Substrate tap. Physical-only.
    Tap,
    /// IO pad — input-only.
    IoPadInput,
    /// IO pad — output-only.
    IoPadOutput,
    /// IO pad — bidirectional (a/oe/y triple).
    IoPadBidir,
    /// Delay buffer (timing-only; behavioural identity).
    Delay,
    /// Multi-output cell (full adder, half adder, dual-output DFF).
    MultiOutput,
    /// Constant-1 driver.
    TieHigh,
    /// Constant-0 driver.
    TieLow,
}

/// Top-level TOML manifest schema (v1.0).
///
/// ```toml
/// schema_version = "1.0"
///
/// [cells.gf180mcu_ocd_ip_sram__sram1024x8m8wm1]
/// kind = "ram"
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct ManifestFile {
    pub schema_version: String,
    #[serde(default)]
    pub cells: HashMap<String, CellEntry>,
}

/// Per-cell manifest entry. In v1.0 the only field is `kind`; future
/// schema versions add `ports = { ... }` and similar.
#[derive(Debug, Clone, Deserialize)]
pub struct CellEntry {
    pub kind: CellKind,
}

/// Runtime cell-library data — pin tables parsed from user-supplied
/// `.v` files, optionally augmented with `kind` metadata from sibling
/// `.cells.toml` manifests.
#[derive(Debug, Default)]
pub struct RuntimeCellLibrary {
    /// `(macro_name, pin_name) → (Direction, Option<width>)`.
    /// `Direction::Unknown` is used for `inout` ports (matching how
    /// built-in providers represent power pins and bidir IO).
    /// `width` is `Some` for bus pins (e.g. `Q[7:0]`), `None` for
    /// single-bit pins.
    pins: HashMap<CompactString, HashMap<CompactString, PinInfo>>,
    /// `macro_name → CellKind`. Populated from manifest TOML files;
    /// pins-only entries (no manifest sibling) leave the kind unset.
    kinds: HashMap<CompactString, CellKind>,
}

/// Direction + optional bus width for a single pin on a runtime cell.
#[derive(Debug, Clone)]
struct PinInfo {
    direction: Direction,
    width: Option<SVerilogRange>,
}

/// Failure to load a runtime cell library — either Verilog or TOML.
#[derive(Debug)]
pub enum LoadError {
    Io {
        path: PathBuf,
        err: std::io::Error,
    },
    VerilogParse {
        path: PathBuf,
        message: String,
    },
    ManifestParse {
        path: PathBuf,
        err: toml::de::Error,
    },
    UnsupportedSchemaVersion {
        path: PathBuf,
        found: String,
    },
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::Io { path, err } => {
                write!(f, "reading {}: {err}", path.display())
            }
            LoadError::VerilogParse { path, message } => {
                write!(f, "parsing {}: {message}", path.display())
            }
            LoadError::ManifestParse { path, err } => {
                write!(f, "parsing manifest {}: {err}", path.display())
            }
            LoadError::UnsupportedSchemaVersion { path, found } => write!(
                f,
                "manifest {} declares schema_version = \"{found}\" — \
                 only \"1.0\" is supported in this build",
                path.display()
            ),
        }
    }
}

impl std::error::Error for LoadError {}

impl RuntimeCellLibrary {
    /// Parse one or more user-supplied `.v` files. Each file is fed
    /// through `sverilogparse`; every `module … endmodule` block
    /// contributes pin-direction entries.
    ///
    /// Sibling `<file>.cells.toml` manifests, if present, are
    /// autoloaded — explicit manifest paths can be added via
    /// [`Self::load_manifest`] afterwards (the explicit-flag wins
    /// because it's applied last).
    ///
    /// Later files (and later manifests) override earlier ones for
    /// collisions and emit a `clilog::warn!` so accidental overlap
    /// surfaces. Built-in PDK providers always win over the
    /// runtime library (see [`ChainedPinProvider`]).
    pub fn from_files(library_paths: &[PathBuf]) -> Result<Self, LoadError> {
        let mut lib = Self::default();
        for path in library_paths {
            lib.load_verilog(path)?;
            let manifest_sibling = with_cells_toml_extension(path);
            if manifest_sibling.exists() {
                lib.load_manifest(&manifest_sibling)?;
            }
        }
        Ok(lib)
    }

    /// Parse a single Verilog file and merge its modules into the
    /// pin table. Called from `from_files`; exposed for tests.
    pub fn load_verilog(&mut self, path: &Path) -> Result<(), LoadError> {
        let bytes = std::fs::read(path).map_err(|err| LoadError::Io {
            path: path.to_path_buf(),
            err,
        })?;
        let parsed =
            SVerilog::parse_u8slice(&bytes).map_err(|message| LoadError::VerilogParse {
                path: path.to_path_buf(),
                message,
            })?;
        for (module_name, module) in &parsed.modules {
            let mut entry: HashMap<CompactString, PinInfo> = HashMap::new();
            for def in &module.defs {
                let direction = match def.typ {
                    WireDefType::Input => Direction::I,
                    WireDefType::Output => Direction::O,
                    // netlistdb treats inout as Unknown (per
                    // src/lib.rs:11 "inout is not supported yet").
                    // Match the built-in providers' behaviour.
                    WireDefType::InOut => Direction::Unknown,
                    WireDefType::Wire => continue,
                };
                entry.insert(
                    def.name.clone(),
                    PinInfo {
                        direction,
                        width: def.width.clone(),
                    },
                );
            }
            if entry.is_empty() {
                continue;
            }
            if self.pins.contains_key(module_name) {
                clilog::warn!(
                    "cell library: module `{module_name}` redefined while parsing {}",
                    path.display()
                );
            }
            self.pins.insert(module_name.clone(), entry);
        }
        Ok(())
    }

    /// Load a TOML manifest into the kind table. Multiple manifests
    /// may target the same library; later wins, with a warning.
    pub fn load_manifest(&mut self, path: &Path) -> Result<(), LoadError> {
        let text = std::fs::read_to_string(path).map_err(|err| LoadError::Io {
            path: path.to_path_buf(),
            err,
        })?;
        let parsed: ManifestFile =
            toml::from_str(&text).map_err(|err| LoadError::ManifestParse {
                path: path.to_path_buf(),
                err,
            })?;
        if parsed.schema_version != "1.0" {
            return Err(LoadError::UnsupportedSchemaVersion {
                path: path.to_path_buf(),
                found: parsed.schema_version,
            });
        }
        for (cell_name, entry) in parsed.cells {
            let key = CompactString::new(cell_name);
            if self.kinds.contains_key(&key) {
                clilog::warn!(
                    "cell library: kind for `{key}` redefined by manifest {}",
                    path.display()
                );
            }
            self.kinds.insert(key, entry.kind);
        }
        Ok(())
    }

    /// True if any cell library files were loaded (helpful for
    /// gating logging / diagnostic paths).
    pub fn is_empty(&self) -> bool {
        self.pins.is_empty() && self.kinds.is_empty()
    }

    /// Number of (module, pin) entries currently in the pin table.
    /// Diagnostic-only.
    pub fn pin_entry_count(&self) -> usize {
        self.pins.values().map(|m| m.len()).sum()
    }

    /// Number of cells with a kind annotation.
    pub fn kind_entry_count(&self) -> usize {
        self.kinds.len()
    }

    /// Lookup a (macro, pin) direction. Returns `None` if the runtime
    /// library has nothing to say — the chained provider's caller
    /// then delegates to its built-in fallback.
    pub fn lookup_pin(
        &self,
        macro_name: &CompactString,
        pin_name: &CompactString,
    ) -> Option<Direction> {
        Some(self.pins.get(macro_name)?.get(pin_name)?.direction.clone())
    }

    /// Lookup a (macro, pin) bus width. Returns `None` for unknown
    /// cells, unknown pins, or single-bit pins.
    pub fn lookup_width(
        &self,
        macro_name: &CompactString,
        pin_name: &CompactString,
    ) -> Option<SVerilogRange> {
        self.pins.get(macro_name)?.get(pin_name)?.width.clone()
    }

    /// Lookup the declared kind for a cell. `None` means the cell
    /// is unknown to the runtime library.
    pub fn lookup_kind(&self, macro_name: &str) -> Option<CellKind> {
        self.kinds.get(macro_name).copied()
    }
}

fn with_cells_toml_extension(path: &Path) -> PathBuf {
    // `foo.v` → `foo.cells.toml`; `foo.bar.v` → `foo.bar.cells.toml`.
    let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned());
    match file_name {
        Some(name) => {
            let stem = name.strip_suffix(".v").unwrap_or(&name);
            path.with_file_name(format!("{stem}.cells.toml"))
        }
        None => path.with_extension("cells.toml"),
    }
}

/// Chained pin-direction provider. The runtime library answers for
/// cells it knows about; everything else delegates to `fallback`.
pub struct ChainedPinProvider<'a, P: LeafPinProvider> {
    pub runtime: &'a RuntimeCellLibrary,
    pub fallback: &'a P,
}

impl<P: LeafPinProvider> LeafPinProvider for ChainedPinProvider<'_, P> {
    fn direction_of(
        &self,
        macro_name: &CompactString,
        pin_name: &CompactString,
        pin_idx: Option<isize>,
    ) -> Direction {
        if let Some(dir) = self.runtime.lookup_pin(macro_name, pin_name) {
            return dir;
        }
        self.fallback.direction_of(macro_name, pin_name, pin_idx)
    }

    fn width_of(
        &self,
        macro_name: &CompactString,
        pin_name: &CompactString,
    ) -> Option<SVerilogRange> {
        if let Some(width) = self.runtime.lookup_width(macro_name, pin_name) {
            return Some(width);
        }
        self.fallback.width_of(macro_name, pin_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use tempfile::TempDir;

    fn write_file(dir: &TempDir, name: &str, content: &str) -> PathBuf {
        let path = dir.path().join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        path
    }

    const OCD_SRAM_VERILOG: &str = "\
module gf180mcu_ocd_ip_sram__sram1024x8m8wm1 (
    CLK, CEN, GWEN, WEN, A, D, Q
);
  input CLK, CEN, GWEN;
  input [7:0] WEN;
  input [9:0] A;
  input [7:0] D;
  output [7:0] Q;
endmodule
";

    #[test]
    fn parses_verilog_pin_directions() {
        let dir = TempDir::new().unwrap();
        let path = write_file(&dir, "ocd_sram.v", OCD_SRAM_VERILOG);
        let mut lib = RuntimeCellLibrary::default();
        lib.load_verilog(&path).unwrap();

        let macro_name =
            CompactString::new("gf180mcu_ocd_ip_sram__sram1024x8m8wm1");
        assert_eq!(
            lib.lookup_pin(&macro_name, &CompactString::new("CLK")),
            Some(Direction::I)
        );
        assert_eq!(
            lib.lookup_pin(&macro_name, &CompactString::new("Q")),
            Some(Direction::O)
        );
        // Unknown pin
        assert_eq!(
            lib.lookup_pin(&macro_name, &CompactString::new("nope")),
            None
        );
        // Unknown macro
        assert_eq!(
            lib.lookup_pin(&CompactString::new("other_cell"), &CompactString::new("CLK")),
            None
        );
    }

    #[test]
    fn parses_inout_as_unknown() {
        // netlistdb's Direction enum has no Inout variant — built-in
        // providers map inout to Unknown. Manifest path must match.
        let dir = TempDir::new().unwrap();
        let path = write_file(
            &dir,
            "powered.v",
            "module powered_cell (VDD, VSS, A);\n  inout VDD, VSS;\n  input A;\nendmodule\n",
        );
        let mut lib = RuntimeCellLibrary::default();
        lib.load_verilog(&path).unwrap();
        let macro_name = CompactString::new("powered_cell");
        assert_eq!(
            lib.lookup_pin(&macro_name, &CompactString::new("VDD")),
            Some(Direction::Unknown)
        );
        assert_eq!(
            lib.lookup_pin(&macro_name, &CompactString::new("A")),
            Some(Direction::I)
        );
    }

    #[test]
    fn parses_manifest_kind() {
        let dir = TempDir::new().unwrap();
        write_file(&dir, "ocd_sram.v", OCD_SRAM_VERILOG);
        write_file(
            &dir,
            "ocd_sram.cells.toml",
            "schema_version = \"1.0\"\n\
             [cells.gf180mcu_ocd_ip_sram__sram1024x8m8wm1]\n\
             kind = \"ram\"\n",
        );
        let lib =
            RuntimeCellLibrary::from_files(&[dir.path().join("ocd_sram.v")]).unwrap();
        assert_eq!(
            lib.lookup_kind("gf180mcu_ocd_ip_sram__sram1024x8m8wm1"),
            Some(CellKind::Ram)
        );
        assert_eq!(lib.lookup_kind("not_there"), None);
    }

    #[test]
    fn rejects_unsupported_schema_version() {
        let dir = TempDir::new().unwrap();
        let manifest = write_file(
            &dir,
            "x.cells.toml",
            "schema_version = \"2.0\"\n",
        );
        let mut lib = RuntimeCellLibrary::default();
        let err = lib.load_manifest(&manifest).unwrap_err();
        assert!(matches!(err, LoadError::UnsupportedSchemaVersion { .. }));
    }

    #[test]
    fn rejects_unknown_kind() {
        let dir = TempDir::new().unwrap();
        let manifest = write_file(
            &dir,
            "x.cells.toml",
            "schema_version = \"1.0\"\n[cells.foo]\nkind = \"quantum\"\n",
        );
        let mut lib = RuntimeCellLibrary::default();
        let err = lib.load_manifest(&manifest).unwrap_err();
        assert!(matches!(err, LoadError::ManifestParse { .. }));
    }

    #[test]
    fn from_files_autoloads_sibling_manifest() {
        let dir = TempDir::new().unwrap();
        write_file(&dir, "ocd_sram.v", OCD_SRAM_VERILOG);
        write_file(
            &dir,
            "ocd_sram.cells.toml",
            "schema_version = \"1.0\"\n\
             [cells.gf180mcu_ocd_ip_sram__sram1024x8m8wm1]\n\
             kind = \"ram\"\n",
        );
        let lib =
            RuntimeCellLibrary::from_files(&[dir.path().join("ocd_sram.v")]).unwrap();
        assert_eq!(
            lib.lookup_kind("gf180mcu_ocd_ip_sram__sram1024x8m8wm1"),
            Some(CellKind::Ram)
        );
        // Pins also populated.
        let macro_name =
            CompactString::new("gf180mcu_ocd_ip_sram__sram1024x8m8wm1");
        assert_eq!(
            lib.lookup_pin(&macro_name, &CompactString::new("Q")),
            Some(Direction::O)
        );
    }

    #[test]
    fn from_files_succeeds_without_manifest_sibling() {
        let dir = TempDir::new().unwrap();
        write_file(&dir, "ocd_sram.v", OCD_SRAM_VERILOG);
        // No .cells.toml — pins-only load should still succeed.
        let lib =
            RuntimeCellLibrary::from_files(&[dir.path().join("ocd_sram.v")]).unwrap();
        assert!(lib.kind_entry_count() == 0);
        assert!(lib.pin_entry_count() > 0);
    }

    #[test]
    fn chained_provider_runtime_wins_then_fallback() {
        struct AlwaysOutput;
        impl LeafPinProvider for AlwaysOutput {
            fn direction_of(
                &self,
                _: &CompactString,
                _: &CompactString,
                _: Option<isize>,
            ) -> Direction {
                Direction::O
            }
            fn width_of(&self, _: &CompactString, _: &CompactString) -> Option<SVerilogRange> {
                None
            }
        }

        let dir = TempDir::new().unwrap();
        let path = write_file(&dir, "x.v", OCD_SRAM_VERILOG);
        let mut runtime = RuntimeCellLibrary::default();
        runtime.load_verilog(&path).unwrap();
        let fallback = AlwaysOutput;
        let chained = ChainedPinProvider {
            runtime: &runtime,
            fallback: &fallback,
        };

        // Runtime knows about CLK on the OCD SRAM — should win.
        assert_eq!(
            chained.direction_of(
                &CompactString::new("gf180mcu_ocd_ip_sram__sram1024x8m8wm1"),
                &CompactString::new("CLK"),
                None,
            ),
            Direction::I
        );
        // Runtime doesn't know about this cell — falls through.
        assert_eq!(
            chained.direction_of(
                &CompactString::new("unknown_cell"),
                &CompactString::new("any_pin"),
                None,
            ),
            Direction::O
        );
    }

    #[test]
    fn cells_toml_extension_helper() {
        assert_eq!(
            with_cells_toml_extension(Path::new("foo.v")),
            PathBuf::from("foo.cells.toml")
        );
        assert_eq!(
            with_cells_toml_extension(Path::new("a/b/foo.bar.v")),
            PathBuf::from("a/b/foo.bar.cells.toml")
        );
        // Non-.v extension — append rather than replace; the
        // function is only invoked on `--cell-library` paths and
        // .v is the dominant case.
        assert_eq!(
            with_cells_toml_extension(Path::new("x.sv")),
            PathBuf::from("x.sv.cells.toml")
        );
    }
}
