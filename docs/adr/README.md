# Architecture Decision Records

ADRs capture decisions worth understanding later: the context, the
options considered, and the rationale for the choice. They are
numbered, append-only, and never silently rewritten — if a decision
changes, supersede the old ADR with a new one and update the status.

## Status legend

- **Accepted / Approved** — current, in effect.
- **Proposed** — drafted, not yet ratified.
- **Superseded** — historical, replaced by a later ADR or by a spike
  outcome; kept for the audit trail.

## Index

| # | Title | Status |
|---|---|---|
| [0001](0001-opensta-as-oracle.md) | OpenSTA as the timing correctness oracle and sole STA path | Accepted (scope expanded 2026-05-01) |
| [0002](0002-timing-ir.md) | Timing intermediate representation | Accepted |
| [0003](0003-opentimer-primary-sta.md) | OpenTimer as in-process reference STA | Superseded (2026-05-01) — spike failed; OpenSTA subprocess only |
| [0004](0004-private-pdk-testing.md) | Private PDK testing track | Accepted |
| [0005](0005-opensta-vendoring-and-corpus.md) | OpenSTA vendoring and test-corpus strategy | Accepted |
| [0006](0006-sdf-preprocessing-model.md) | SDF preprocessing model and interim-to-release cutover | Accepted (amended 2026-05-02) |
| [0007](0007-timing-model-fidelity-roadmap.md) | Timing model fidelity roadmap | Proposed |
| [0008](0008-structured-timing-output.md) | Structured timing output as first-class deliverable | Approved |

## How the ADRs relate

- **0001 / 0003 / 0005 / 0006** describe the timing oracle stack:
  OpenSTA as the ground truth (0001), vendored at a pinned revision
  with its own corpus reused (0005), driving SDF preprocessing
  out-of-process (0006). The earlier OpenTimer in-process plan (0003)
  was retired after the spike ([`../spikes/opentimer-sky130.md`](../spikes/opentimer-sky130.md)).
- **0002** is the data contract those tools talk over — a JSON timing
  IR consumed by Jacquard, produced by `opensta-to-ir`.
- **0004** governs how PDK-specific testing happens for NDA-bound
  contributors without leaking files into the public repo.
- **0007 / 0008** are the forward-looking pair: 0008 (Approved)
  defines the structured timing output Jacquard owes downstream
  flows; 0007 (Proposed) sketches the model-fidelity work needed to
  back those outputs at scale (δ(T), clock-tree skew, wire delay).
  Scheduling for both lives in
  [`../plans/post-phase-0-roadmap.md`](../plans/post-phase-0-roadmap.md).

## Adding a new ADR

1. Pick the next number (highest existing + 1).
2. Filename: `NNNN-short-kebab-title.md`.
3. Start with `# ADR NNNN — <title>` and a `**Status:**` line.
4. Standard sections: Context, Decision, Consequences. Add Amendment
   blocks dated when the decision is revisited; do not rewrite
   accepted history.
5. Add the row to the table above.
