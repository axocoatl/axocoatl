# Deterministic demo scenarios

Each scenario is an immutable repository fixture copied into its own marked
temporary root. Preparation creates a fresh Git repository and a fresh Axocoatl
data directory, then proves that the fixture has exactly its intended initial
red checks. Do not repair files below `workspace-template/` or `fixtures/`; make
product changes only in the prepared temporary workspace.

| Scenario key | Fixture | Initial check contract | Best demonstration |
| --- | --- | --- | --- |
| `northstar-storefront` | `workspace-template/` | 6 tests: 5 pass, 1 fails on the negative payable invariant | single Agent, durable Turns, Terminal, Preview, and Git |
| `harbor-catalog` | `fixtures/harbor-catalog/` | 6 tests: 3 pass, 3 fail on cache coherence after mutation | Several Ways with genuinely different valid strategies; architect → reviewer **Film · Handoff sequence** handoff |
| `signal-desk` | `fixtures/signal-desk/` | 5 tests: 3 pass, 2 fail on incident correlation and severity | event-lattice and Automation scenarios |

## Prepare

The no-argument command preserves the original Northstar behavior and root:

```bash
./demo/one-app/prepare.sh
```

Prepare a named scenario with an isolated root:

```bash
./demo/one-app/prepare.sh --scenario harbor-catalog
./demo/one-app/prepare.sh --scenario signal-desk
```

Preparation prints the exact `AXOCOATL_DEMO_ROOT=… start.sh` command needed for
the named root. On macOS the defaults are below `/private/tmp`; on Linux they are
below `/tmp`. An explicit `AXOCOATL_DEMO_ROOT` remains supported, subject to the
existing direct-child and `axocoatl-one-app-showcase*` safety rules.

The command refuses a live demo port, any surviving `axo-ses-*` container, an
unmarked destination, a symlinked destination or fixture, and a fixture whose
intentional check contract no longer matches. An older marked root is moved to a
timestamped backup only after prerequisite and image checks succeed.

## Verify without preparing again

Use the verifier after a rehearsal reset or before recording:

```bash
./demo/one-app/verify-scenario.sh \
  harbor-catalog \
  /private/tmp/axocoatl-one-app-showcase-harbor-catalog/workspace
```

The verifier checks the package identity, expected failure names, exact
test/pass/fail totals, and zero cancelled/skipped/todo tests. Its default log is
stored inside `.git`, so verification does not dirty the prepared workspace.
It intentionally exits nonzero if the scenario has already been repaired or
fails for a different reason.

## Recording prompts

Use one task per film so the runtime shape remains legible.

### Harbor Catalog — Several Ways

```text
Repair the catalog cache-coherency defect. Search results must reflect additions,
updates, and removals after a query has been cached. Preserve the public API and
caching, do not change tests, run npm run check, and report Diagnosis / Decision /
Evidence / Tradeoff.
```

The useful comparison is the route: broad invalidation, targeted invalidation,
and revisioned keys can all satisfy the same contract. Checks and the operator
decide whether an implementation is acceptable.

### Harbor Catalog — multi-agent design handoff

```text
Architect: write exactly one sentence proposing how to prevent stale catalog cache reads after mutations; do not speak for the reviewer. Reviewer: assess only the architect sentence in exactly one sentence and end with SHIP or BLOCK. No tools, files, or edits.
```

Create the **Film · Handoff sequence** Custom Session with exactly Systems Architect and
Critical Reviewer, with reviewer dependent on architect. This proves one sequential
configured handoff through its dependency edge, ordered live activation, and separate
durable outputs. It is not parallel Ways and does not prove repository repair or code
correctness.
