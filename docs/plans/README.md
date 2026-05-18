# Implementation Plans

Phased implementation plans with entry and exit criteria. Plans live
here when the work spans multiple commits and needs an explicit
scheduling artefact; once shipped, the plan is kept as a historical
record (Status flipped to *Implemented*) rather than deleted, so the
phasing is recoverable later.

For short-lived working memory between sessions, see
[`../handoff-discipline.md`](../handoff-discipline.md) — that lives
in `docs/handoffs/` and is deliberately kept separate from the
persistent plans here.

## Status legend

- **Active** — currently being worked on or scheduled.
- **Implemented** — shipped; kept as historical record.
- **Deferred** — captured for future work; not currently scheduled.
- **Exploratory** — architectural thinking captured ahead of demand.

## Index

| Plan | Status |
|---|---|
| [Post-Phase-0 Roadmap](post-phase-0-roadmap.md) | Active — scheduling doc for ADRs 0007 and 0008 |
| [GF180MCU PDK enablement](gf180mcu-enablement.md) | Mostly implemented — Phases 0–6 shipped; Phase 7 deferred |
| [Phase 0: Timing IR and OpenSTA oracle](phase-0-ir-and-oracle.md) | Implemented — historical record |
| [WS2: `opensta-to-ir`](ws2-opensta-to-ir.md) | Implemented — historical record |
| [WS3: delete SDF parser + interim runtime hook](ws3-delete-sdf-parser.md) | Implemented — historical record (see ADR 0006 Amendment) |
| [WS3 follow-up: re-add cosim `--sdf` via `opensta-to-ir`](ws3-cosim-sdf-followup.md) | Deferred |
| [Multi-clock and stimulus architecture](multi-clock-and-stimulus-architecture.md) | Exploratory — demand-driven |

## Reading order for new contributors

If you want to understand how the timing stack got to where it is:

1. [`phase-0-ir-and-oracle.md`](phase-0-ir-and-oracle.md) — the
   umbrella plan, with the five work streams (WS1–WS5).
2. [`ws2-opensta-to-ir.md`](ws2-opensta-to-ir.md) and
   [`ws3-delete-sdf-parser.md`](ws3-delete-sdf-parser.md) — the
   per-work-stream detail for the IR producer and the SDF parser
   removal.
3. [`post-phase-0-roadmap.md`](post-phase-0-roadmap.md) — what comes
   next, sequenced against the ADRs.

## Adding a new plan

1. Filename: short kebab-case (`<topic>.md` or
   `<ws-or-phase>-<topic>.md`).
2. Start with `# Plan — <title>` and a `**Status:**` line.
3. Where the plan executes a specific ADR or work stream, name them
   in a `**Predecessors:**` / `**ADRs:**` block near the top so the
   dependency graph is explicit.
4. Add the row to the table above. When the plan ships, change the
   status in the file and here in the same commit.
