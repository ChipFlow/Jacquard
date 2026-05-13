# Release Process

Lightweight by design. Jacquard is a single-binary Rust project with
`vendor/` submodules; releases are git tags + a CHANGELOG entry. No
crates.io publication, no pre-built binaries (until/unless that
demand surfaces).

## When to release

Cut a release when:

- A user-visible feature or fix lands that you want consumers to be
  able to pin against.
- Schema or CLI changes happened (`--timing-report` JSON, CLI flags)
  and consumers need a stable reference point.
- A meaningful chunk of work in `docs/plans/` is closed (e.g. a Phase
  exits all criteria).

There is no fixed cadence.

## Versioning

[SemVer](https://semver.org/), starting once the first numbered
release ships. Pre-1.0 versions (`0.x.0`) carry the standard SemVer
caveat: minor bumps may include breaking changes; the public
contracts (`--timing-report` schema, IR layout) are documented in their
own ADRs and follow stricter rules.

Stable contracts (additive-only, breaking changes require a major
bump and a deprecation window):

- `--timing-report` JSON schema — `src/timing_report.rs::SCHEMA_VERSION`,
  governed by ADR 0008.
- Timing IR FlatBuffers schema — `crates/timing-ir/schemas/timing_ir.fbs`,
  governed by ADR 0002.

CLI flags, log message formats, and `--timing-summary` text output are
**not** stable parseable contracts; consumers that need to script
against them should use `--timing-report` JSON.

## Steps

For maintainers cutting a release:

1. **Verify CI is green** on `main` for all three GPU backends (Metal,
   CUDA, HIP) plus the unit-test, opensta-to-ir, and lint jobs. If any
   GPU runner is offline, hold the release until it's restored — see
   [`.github/workflows/ci.yml`](../.github/workflows/ci.yml). Do not
   ship a binary the CI hasn't built.

2. **Roll the `[Unreleased]` section in [`CHANGELOG.md`](../CHANGELOG.md)
   into a numbered version block.** Format follows
   [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Update
   the link references at the bottom of the file. Leave a fresh empty
   `[Unreleased]` section at the top.

3. **Bump `version` in [`Cargo.toml`](../Cargo.toml)** to match.
   `cargo build` to update `Cargo.lock`.

4. **Commit:** `chore: release v<X.Y.Z>` with the standard
   `Co-developed-by` trailer.

5. **Tag:** `git tag -a v<X.Y.Z> -m "v<X.Y.Z>"` then `git push --tags`.

6. **Create a GitHub release** from the tag. Body = the CHANGELOG
   section for that version. No artefacts attached unless someone has
   asked for them.

## What does NOT need to change at release time

- Submodule pins (unless deliberately bumping a vendored dep).
- The `vendor/opensta/` submodule pin is the version named in
  `crates/opensta-to-ir::MIN_TESTED_OPENSTA_VERSION`. If you bump the
  submodule, also bump the constant and the version-probe test — see
  WS-RH.1 in [`docs/plans/post-phase-0-roadmap.md`](plans/post-phase-0-roadmap.md).
- `LICENSE` (unless re-licensing).

## Pre-release checklist (one-time, before the first numbered release)

These items are tracked in [`docs/plans/post-phase-0-roadmap.md`](plans/post-phase-0-roadmap.md)
§ Release hardening; this section is the visible punch-list:

- [x] Phase 1 (ADR 0008 required outputs) closed.
- [x] WS-RH.1 (OpenSTA detection + version check) shipped.
- [x] Metal CI on `macos-runner-1` green (re-enabled in commit `12e98df`,
      2026-05-12).
- [ ] **CUDA CI** on `nvidia-runner-1` green on main. Currently
      disabled in `.github/workflows/ci.yml` (`if: ${{ false }}`,
      ~line 268). Re-enable when hardware lands.
- [ ] **HIP CI** on the AMD runner green on main. Currently disabled
      in `.github/workflows/ci.yml` (`if: ${{ false }}`, ~line 357).
      Re-enable when the AMD runner is online.
- [x] Vendored-dep license posture confirmed
      ([gzz2000/eda-infra-rs#2](https://github.com/gzz2000/eda-infra-rs/issues/2#issuecomment-4363789319)
      — sverilogparse AGPL declaration acknowledged as a typo; workspace
      Apache-2.0 governs).
- [x] `Cargo.toml::license = "Apache-2.0"` set.
- [x] `NOTICE` file enumerating vendored deps + their licenses.
- [ ] **Bump `vendor/eda-infra-rs` submodule** once upstream pushes the
      sverilogparse `Cargo.toml` correction; remove the corresponding
      footnote in `NOTICE`. Maintainer acknowledged the typo on
      2026-05-02 but hasn't pushed the fix as of 2026-05-13. Verify
      with `git -C vendor/eda-infra-rs fetch && git log origin/master --oneline`.
- [ ] **CUDA / HIP runtime violation routing** through
      `process_events` (or document loudly that `--timing-report` is
      Metal-only at release time). `sim_cuda` / `sim_hip` accept
      `timing_constraints` but currently call the simple-scan path
      instead of the timed-batched path. Blocked on the same GPU
      runners as the CUDA/HIP CI jobs above.
- [x] Bounded violations array (`--timing-report-max-violations`,
      default 100k).
- [ ] **End-to-end `--timing-report` test on Metal CI.** Now
      unblocked: `macos-runner-1` is live and the sky130 multi-corner
      test passes there. Wire a small `--timing-report` test against
      `benchmarks/dataset/nvdla*` (or any small corpus design) in
      the `metal` CI job; validate the JSON shape against
      `tests/timing_ir/sample_reports/two_violations.json`.
- [x] GF180MCU enablement (Phases 0–6) shipped. See
      `docs/plans/gf180mcu-enablement.md`. Phase 7 (wafer.space
      test-run-1 design integration) deferred pending design
      availability; not release-blocking.

## License posture

Project license is Apache-2.0 (`LICENSE`). Vendored-dep posture is
enumerated in `NOTICE`. Summary:

- `vendor/eda-infra-rs/` — Apache-2.0 (workspace). The `sverilogparse`
  sub-crate's stale `AGPL-3.0-only` declaration in `Cargo.toml` is a
  typo per upstream maintainer
  ([gzz2000/eda-infra-rs#2](https://github.com/gzz2000/eda-infra-rs/issues/2#issuecomment-4363789319));
  governed by the workspace LICENSE. Submodule pin will be bumped when
  upstream pushes the correction.
- `vendor/sky130_fd_sc_hd/` — Apache-2.0.
- `vendor/opensta/` — GPL-3 (subprocess only per ADR 0001 + ADR 0006
  § Amendment; never linked, never bundled).

## Cross-references

- [`CHANGELOG.md`](../CHANGELOG.md) — release log.
- [`docs/adr/0008-structured-timing-output.md`](adr/0008-structured-timing-output.md)
  — `--timing-report` stability contract.
- [`docs/adr/0002-timing-ir.md`](adr/0002-timing-ir.md) — IR schema
  versioning.
- [`docs/adr/0006-sdf-preprocessing-model.md`](adr/0006-sdf-preprocessing-model.md)
  — OpenSTA bundling rules.
- [`docs/project-scope.md`](project-scope.md) — license posture
  contract.
