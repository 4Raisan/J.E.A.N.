# PLANNING

> Planning and decision documents for J.E.A.N.
> **This folder is not technical documentation.** It contains no API specs, no code, no feature designs.

---

## Why this folder exists

The project root holds the *technical* documents — the vision, the engineering direction, and the ecosystem research. This folder holds the *planning* documents — what we are committing to build, with which exact tools, configured how, in what order, and what we decided along the way.

If a document answers **"how does it work?"** it belongs in the root or in `docs/`.
If a document answers **"what are we doing, with what, and why?"** it belongs here.

## Root documents (technical — not in this folder)

| Document | What it is |
|---|---|
| [`../Goal - README.md`](../Goal%20-%20README.md) | Product vision, principles, capability areas, stage roadmap |
| [`../IMPLEMENTATION_GUIDE.md`](../IMPLEMENTATION_GUIDE.md) | Engineering direction: architecture, language choice, build order, repo layout, performance targets |
| [`../RESEARCH.md`](../RESEARCH.md) | Open-source ecosystem survey (verified 2026-11-04) — every candidate repo, license, and rejection reason |
| [`../docs/desktop-control-plan.md`](../docs/desktop-control-plan.md) | Proposed Phase-4 plan for capture-unavailable apps, Telegram compatibility, and keeping other desktop tasks usable |

## This folder

| # | Document | Read it when you need to know… |
|---|---|---|
| — | [`01-SCOPE.md`](01-SCOPE.md) | What J.E.A.N. is and is not. Capability boundaries, non-goals, the one workflow that proves the product. |
| — | [`02-TOOLCHAIN.md`](02-TOOLCHAIN.md) | **What tools we use.** Rust toolchain, build prerequisites, dev tooling, external binaries, worker processes. |
| — | [`03-DEPENDENCIES.md`](03-DEPENDENCIES.md) | **Everything we need.** Full inventory of crates, binaries, models and services — with versions, licenses and adopt/avoid verdicts. |
| — | [`04-SETTINGS.md`](04-SETTINGS.md) | **How it is configured.** Every config file, every key, its default, and what it controls. Permission tiers included. |
| — | [`05-ROADMAP.md`](05-ROADMAP.md) | **What we implement, in what order.** Phases, deliverables, exit criteria, dependencies. |
| — | [`06-RISKS.md`](06-RISKS.md) | Verified gaps, licensing traps, Windows hazards, and the fallback for each. |
| — | [`07-DECISIONS.md`](07-DECISIONS.md) | The decision log. What we chose, what we rejected, and why — so the same argument does not happen twice. |

The [owner workflow and personalization milestone](08-OWNER-WORKFLOW.md) defines the planned experience for short goals, learned preferences, agent handoffs, and automatic recovery.

Start current implementation work with the [Luna execution plan](09-LUNA-EXECUTION-PLAN.md). It reconciles source progress, sets the open-source adoption gate, and orders the next batches. Earlier phase records remain the evidence archive.

## Reading order

- **New to the project:** `01-SCOPE` → `05-ROADMAP` → `02-TOOLCHAIN`
- **About to write code:** `03-DEPENDENCIES` → `04-SETTINGS` → `06-RISKS`
- **About to change a decision:** `07-DECISIONS` first — the reasoning is already written down

## Ground rules for this folder

1. **Every version, license and URL here was verified against a live source** (crates.io API, npm registry, PyPI, `gh api`, or official docs) on **2026-11-04**. Nothing is carried over from memory or from the research report without re-checking.
2. **Unverified items are marked `UNVERIFIED`.** They are not treated as facts and must not be planned around.
3. **A dependency is not adopted until it appears in `03-DEPENDENCIES.md` with a verdict.** Candidate ≠ adopted.
4. **Decisions get logged in `07-DECISIONS.md` when they are made**, not reconstructed later.
5. **This folder does not duplicate technical specs.** If you find yourself writing an API contract here, it belongs in `docs/`.

---

*Last reviewed: 2026-11-04*
