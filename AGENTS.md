# Repository Guidelines

## Project Structure & Module Organization

`hinagata` is a Rust 2024 crate. Registry for classical ML models: versions, signatures, lineage, stages, deployment descriptors and drift monitoring. Source lives in `src/`. Integration
tests go in `tests/`. Read `VISION.md` (vision and roadmap), `README.md`, and
accepted ADRs under `docs/decisions/` before changing system boundaries.
Operator docs live in `docs/` (JSON schemas under `docs/schemas/`); the API
contract is `proto/hinagata/v1`. A new ADR is the next numbered file following
`0001-embedded-registry-store.md` (there is no template file) and is added to
the `docs/decisions/README.md` index.

Affected areas used in Issues and impact notes: api, auth, registry,
artifacts, signatures, lineage, promotion, descriptors, drift, quality,
bundles, and ops (configuration, CLI, metrics, diagnostics, and the static
image).

## Build, Test, and Development Commands

- `cargo fmt` formats Rust code before review.
- `cargo test` runs the test suite.
- `cargo clippy --all-targets --locked -- -D warnings` matches CI.
- `make validate`, `make test` and `make test-integration` are the local gates.
  They match the build, test, fmt and clippy jobs in `.github/workflows/ci.yml`:
  `cargo build --all-targets --locked`, `cargo test --locked`, `cargo fmt --check`,
  and `cargo clippy --all-targets --locked -- -D warnings`. CI also runs
  `cargo deny check licenses`, the S3-compatible artifact store job, and the
  static image.

GitHub Issues are the planning source of truth. Workflow Skills are shared;
see `## Shared agent skills`. Never commit hostnames, home paths, secrets or
private environment details.

## Agent delivery closeout (required)

`cargo test` / Clippy / green CI are **not** a substitute for structured code
review. For non-trivial implementation work (any behavior, security, settings,
or public-API change) that will be **committed, pushed, or opened as a PR**:

1. Run `make update`, `make validate`, `make test`, and
   `make test-integration` via the `verify-change` Skill.
2. Run **`autoreview`** before the ship commit (or before push if the commit
   already exists). **Do not vendor** the autoreview skill into this repo;
   resolve the helper from the shared skill install (first match wins):

   ```bash
   # Preferred: shared agent-skills checkout or global skills home
   if [ -x "${AUTOREVIEW:-}" ]; then :; \
   elif [ -x "$HOME/Projects/agent-skills/skills/autoreview/scripts/autoreview" ]; then
     export AUTOREVIEW="$HOME/Projects/agent-skills/skills/autoreview/scripts/autoreview"
   elif [ -x "${AGENTS_HOME:-$HOME/.agents}/skills/autoreview/scripts/autoreview" ]; then
     export AUTOREVIEW="${AGENTS_HOME:-$HOME/.agents}/skills/autoreview/scripts/autoreview"
   else
     echo "autoreview helper not found; install shared agent-skills or set AUTOREVIEW" >&2
     exit 1
   fi
   # Dirty uncommitted work:
   "$AUTOREVIEW" --mode local
   # Topic branch / open PR (preferred after commit):
   "$AUTOREVIEW" --mode branch --base origin/main
   # Already committed on a clean tree:
   "$AUTOREVIEW" --mode commit --commit HEAD
   ```

3. Treat helper output as advisory: verify each accepted finding in the real
   code, fix actionable ones, rerun tests and autoreview until the helper exits
   0 with no accepted/actionable findings (or document a maintainer judgment
   blocker in the PR).
4. Report in the PR or handoff: commands run, tests, autoreview result
   (clean / findings fixed / consciously rejected).

**Do not skip autoreview** because CI is green, the change is “small,” or the
session is optimizing for throughput unless the user explicitly waived it.
Docs-only typo fixes and pure formatting may skip structured review; state the
waiver. The `deliver-ready-issue` Skill requires this same closeout before
publish/land.

Default review engine is Codex (`gpt-6.1-sol` via the helper). Read the shared
`autoreview` skill’s `SKILL.md` next to the resolved helper for engines and
findings policy.

## Parallel delivery lanes

A delivery lane is one Issue, one branch, one isolated checkout, one Pull
Request, and one owner. Several agents may deliver several Issues at the same
time, on one machine or on many, only as separate lanes. One Issue is never
split across lanes, and one lane never carries a second Issue.

### Claims live on GitHub

Machines cannot see each other's worktrees, so a claim is only what GitHub
shows. An Issue is claimed, and therefore active, when any of the following
exists:

1. an open Pull Request, draft or ready, that references the Issue;
2. a branch `<type>/<issue>`, or any branch matching `*/<issue>-*`;
3. the authenticated login assigned to the Issue less than six hours ago.

An assignment older than six hours with neither branch nor Pull Request is an
ownership hint, not a veto: report it and ask the maintainer before claiming.
A claim is stale once its Issue is closed or its Pull Request has merged; its
owner or the maintainer removes it. Any other takeover first preserves the
existing branch, Pull Request, and evidence and requires the maintainer's
confirmation, unless the lead of the same parallel run owns the stalled lane.

A lane claims before it implements, under Publish or Land authority or with an
explicit claim authorization under Implement, using
`bash "${AGENT_KIT_HOME:-$HOME/.agents/agent-kit}"/skills/deliver-ready-issue/scripts/issue-lane.sh claim <issue>`:

- the branch `<type>/<issue>` is created on GitHub from the default branch by
  an atomic ref creation, so when two machines race exactly one claim
  succeeds and the other sees `claimed`;
- the authenticated login is assigned to the Issue, which shows the claim in
  the Issue list and timestamps it in the Issue timeline.

`<type>` is derived deterministically so that every machine computes the same
branch: the Conventional Commit type in the Issue title, with `bug` mapped to
`fix`; otherwise the type label (`bug` → `fix`, `enhancement` → `feat`,
`documentation` → `docs`); otherwise `chore`. Lane branches carry no slug
because the name is the claim; the Pull Request title carries the description.
Human topic branches may keep `<type>/<issue>-<slug>` and are detected as
claims by the same rule, as are legacy `codex/<issue>-<slug>` branches.

An Implement-only lane cannot claim and is invisible to other machines. Say so
in the report, or ask for claim authority before starting.

### Isolation on each machine

Local isolation protects lanes that share a machine; it is not a claim.

- Each lane checks out its claim branch in `.worktrees/issue-<issue>` inside
  the repository, ignored by Git. A fresh clone dedicated to the lane, as on a
  cloud agent, is equivalent. The base SHA is recorded in the lane report.
- The primary checkout belongs to the maintainer. Agents never switch its
  branch, reset it, stash it, or run long jobs in it while another lane is
  active.
- A worktree serves exactly one Issue. Finished lanes are removed; a worktree is
  never reused for a different Issue or renamed to hide its origin.
- Shared Git state is mutated in short, serialized slots: `git fetch --prune`,
  worktree creation and removal, local branch deletion, and merging belong to
  the lead of a parallel run or happen one lane at a time on a shared machine.
  A lane commits and publishes its own branch from its own worktree; that is
  not a shared mutation. Nobody holds a slot across implementation, test runs,
  or a remote wait.

### Limits, collisions, and landing

The three-lane limit counts claims visible on GitHub per repository: open
implementation Pull Requests plus claim branches without one. Assigned or
planned work with neither is not a running lane.

Parallel lanes must not collide. Collision surfaces in this repository are
the API listener and proto, auth hook, registry store, artifact store,
signing keys, promotion hook, descriptors, drift and quality, and bundles.
Lanes that would both change one of these surfaces run in sequence, not in
parallel.

A lane publishes its first Verified commit to the claim branch as a draft Pull
Request that closes the Issue. Public GitHub lane fields may list only: claim
branch, repo-relative worktree (for example `.worktrees/issue-N` — never an
absolute path), base SHA, and published SHA. Keep absolute paths and
machine/host names in the private agent session only. Never put hostnames,
FQDNs, home directories, LAN or employer network names, or other private
environment inventory on the Pull Request. It marks the Pull Request ready
when verification and review are complete. Immediately before publishing, the
lane fetches and confirms that the default branch is an ancestor of its head;
it refreshes onto `main` only for a conflict, a failing gate, an explicit
request, or a sibling landing on a shared surface, not merely because `main`
advanced.

Lanes publish with `issue-lane.sh push <issue>` and land with
`issue-lane.sh land <issue> --pr <pr>` (see `### Verified commits on GitHub`).
`land` merges only the reviewed head once its checks pass and reconciles an
uncertain merge response from the Pull Request state, so it is safe to rerun;
a lane that conflicts with a sibling's landing stops instead of merging. After
each merge, fetch, recompute the frontier, and let the remaining lanes recheck
`mergeable` against the new `main`.

The lead of a parallel run keeps a ledger per lane: Issue, branch, base SHA,
owner, state, Pull Request, evidence, blockers, and cleanup. Keep hostnames
and absolute checkout paths in the session with the maintainer, never on
GitHub. Report verified outcomes, not launched work. The executable lead
procedure is `references/parallel-delivery.md` in the shared
`deliver-ready-issue` skill.

## Coding Style & Naming Conventions

Follow standard Rust formatting with `cargo fmt` and keep modules aligned with
the existing domain boundaries. Use `snake_case` for files, modules, functions,
and variables; use `PascalCase` for types and traits; use
`SCREAMING_SNAKE_CASE` for constants. Prefer explicit fail-closed checks
(missing credentials, unsigned manifests, digest mismatch, schema drift) over
silent degradation. Keep the binary thin; put logic in the library so tests
and hosts can embed the registry without shelling out.

## Testing Guidelines

Add focused tests for changes touching the registry, artifacts, signatures,
lineage, promotion, descriptors, drift, quality, bundles, or operations.
Prefer deterministic tests that do not require an object store. Offline
`cargo test` must stay green without the S3 fixture.
The gRPC/HTTP API and multi-component behavior are covered by integration
tests under `tests/`; configuration changes get parsing and default tests.
Keep service-dependent tests ignored unless their prerequisites and
credentials are intentionally available.

## Commit & Pull Request Guidelines

Use short imperative subjects, often Conventional Commit style:
`feat(registry): refuse a digest mismatch on complete`,
`docs: document signed descriptors`,
`fix(auth): fail closed without a namespace claim`. Keep commits
narrow and describe the affected subsystem when useful. Pull requests should
include a concise behavior summary, **tests run**, **autoreview result** (for
non-trivial code), linked issue or context, and any configuration or security
implications. Do not put hostnames, FQDNs, home directories, absolute worktree
paths, LAN or employer network names, or other private environment inventory on
public Issues, Pull Requests, comments, or commit messages. Release notes
belong in the pull request body; [CHANGELOG.md](CHANGELOG.md) is release-owned.

### Verified commits on GitHub

Publish and land lanes through the shared agent-kit lane tool so GitHub shows
**Verified** commits and `main` stays linear:

1. Implement and commit locally as usual. Never pass `--no-gpg-sign`; if GPG
   fails, stop and fix it.
2. Publish the claim branch from the lane worktree:
   `bash "${AGENT_KIT_HOME:-$HOME/.agents/agent-kit}"/skills/deliver-ready-issue/scripts/issue-lane.sh push <issue>`.
   It creates GitHub-signed commits and resets the worktree to the published
   commit, so local `HEAD` is the Pull Request head. Confirm it reports
   `verification.verified=true`.
3. Land with `bash "${AGENT_KIT_HOME:-$HOME/.agents/agent-kit}"/skills/deliver-ready-issue/scripts/issue-lane.sh land <issue> --pr <pr>`.
   It squash-merges the reviewed head once the required checks pass, and
   deletes the claim branch only after the merge and the closed Issue are
   confirmed.

Do not queue GitHub auto-merge, and do not use GitHub **rebase** merges:
rebase-merge rewrites commits and drops signatures. Do not rewrite protected
`main` after merging unless the user explicitly approves; if protection is
temporarily relaxed, restore force-push and status-check settings immediately
after the correction.

## Security & Configuration Tips

Never commit secrets, tokens, provider credentials, logs, or local state
(`hinagata.db`, artifact copies, JWKS, token files, signing seeds). Never put
hostnames, FQDNs, home directories, absolute worktree paths, LAN or employer
network names, or other private environment inventory on public Issues, Pull
Requests, comments, or commit messages. Report suspected bypasses privately
(GitHub private vulnerability reporting or the maintainer), never in a public
Issue.

## Shared agent skills

The workflow Skills (`advance-issue-frontier`, `assess-change-impact`,
`capture-project-decision`, `deliver-ready-issue`, `prepare-release`,
`refactor-docs`, `shape-work-item`, `technical-documentation`, `verify-change`)
are canonical in [Sannrox/agent-kit](https://github.com/Sannrox/agent-kit).
There is no pin: this repository always uses the latest `main`.
`.agents/skills/<name>/` holds only a generated stub that points to the shared
skill; read the installed skill under `~/.agents/agent-kit/` (or
`$AGENT_KIT_HOME`). Run `bash scripts/validate-workflow-skills.sh` to clone or
update it.

- Repository-specific facts the shared Skills need come from this file: the
  structure, build, parallel-lane and testing sections, plus
  `## Agent skill profile` below for anything they do not already say. Edit
  this file, not the stubs.
- Never edit stubs or the vendored `scripts/validate-workflow-skills.sh` here;
  change them upstream in agent-kit, then run
  `bash scripts/validate-workflow-skills.sh --render` here when CI reports
  drift.
- Repository-only Skills live next to the stubs as full folders and stay local.

## Agent skill profile

Facts the shared agent-kit skills read that the sections above do not already
state.

Feature: a new observable operator or integration outcome.

### Compatibility and failure obligations

Old clients and configuration, SQLite `user_version` migrations, artifact
digest and signed envelope formats, and error semantics stay compatible.
Fail-closed paths stay closed: missing credentials, digest mismatch, unsigned
import, and schema drift.

### Additional impact boundaries

- registry metadata versus artifact bytes;
- JWT/JWKS-file auth versus a static token;
- signed manifests versus unsigned versions;
- caller-governed approval hook versus local stage changes;
- online/batch descriptors versus the runtime that runs them;
- prediction summaries and labeled aggregates versus stored baselines;
- local disk versus S3-compatible artifact store;
- gRPC and HTTP/JSON versus `proto/hinagata/v1`;
- configuration, CLI (`verify`/`export`/`import`), metrics, diagnostics, and
  the static image.

### Verification matrix

Rows beyond `## Testing Guidelines`. The Makefile is the source of truth for
the local gates (`make update`, `make validate`, `make test`,
`make test-integration`); do not reproduce them from prose.

| Surface | Required evidence |
| --- | --- |
| gRPC / HTTP API | affected integration test under `tests/` plus the normal suite |
| configuration | parsing/default tests, README |
| docs / Skills | links and commands; `bash scripts/validate-workflow-skills.sh` |

### Release audit

- `Cargo.toml` version and lockfile consistency;
- proto, gRPC/HTTP, SQLite schema, and signed envelope/bundle format
  compatibility;
- configuration additions, removals, defaults, and README;
- security, rollback, and operator actions;
- static image packaging (`Dockerfile`);
- `CHANGELOG.md` is release-owned: draft its section from
  `scripts/changelog-draft.sh` (merged changes since the last tag) and the
  pull request bodies, upgrade actions first.
