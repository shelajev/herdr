# Fork compatibility inventory

`shelajev/herdr` is a custom fork of `herdrdev/herdr`. It exists to add one
thing upstream does not have: `herdr task`, which runs a crew of coding agents
inside a Docker Sandbox. Everything in this file is a deviation from upstream
that has to survive the next merge, together with the condition that would let
us delete it.

The preceding upstream sync was tested at source commit `5da0a01e1eedda054db0c81dd3a780000c40d9f0` merged into
the fork — that is upstream's master as of this sync, not a cherry-pick.

The model pins, handoff ledger and QC v2 described below are candidate source
changes documented on 2026-10-04. They have not been published or installed by
this work. Historical observations and future promotion steps are separated here.

## Five operations people confuse

Syncing the source changes nothing that is running. These are separate acts,
and doing one does not do the others:

1. **Source sync.** Merging upstream into this repository. Produces commits.
   Changes no installed artifact anywhere.
2. **Host fork build and install.** Building a `herdr` binary from a fork commit
   and installing it as the host task driver. This is the only thing that
   changes what `herdr task` does on your machine.
3. **Inner Herdr runtime.** The `herdr` running *inside* a crew sandbox. It is
   an ordinary upstream build and does not need the task commands. It can be
   pinned to an upstream release independently of anything here.
4. **Template image rebuild.** Rebuilding `docker.io/olegselajev241/herdr-crew`,
   the image the kit boots. Tool versions live here.
5. **Kit republish.** Pushing `docker.io/olegselajev241/herdr-crew-kit`. This is
   what changes `spec.yaml`, the role briefs, and the network policy for new
   sandboxes.

Editing `kits/herdr-crew/spec.yaml` in this repository does not change a running
sandbox, and it does not change the next sandbox either. Only a republish does.
Existing sandboxes keep the kit they were created from for their whole life.

### Host state at the time of this sync

Verified on the host, 2026-10-03:

| Thing | Value |
|---|---|
| Installed host task driver | `/Users/shelajev/.local/bin/herdr-dev`, reports `herdr 0.8.2` |
| Its source | `dd4e3770` plus uncommitted repairs — not a clean source commit |
| Its SHA256 | `1f000cfe4d0380ee2d17a46cd38f5ff641984b110ecc1f90c30a64dffe2af39e` |
| Kit version in this source tree | 0.5.1 — kit **not published**, image published for linux/arm64 (0.5.0 is published and immutable) |
| Published kit | `herdr-crew-kit:latest` = version 0.4.5 |
| Published kit digest (`:latest`) | `sha256:dbeb8b7c4ba2441d3f21d61430e9b5595fdc10f42ef95071d0a0890121de3afe` |
| Published kit digest (`:0.4.5`) | `sha256:4b28f8d50e028d6bbb8407a14dc1a7d0f549ce236338d8b2fd7dd5afe3594753` |
| Template image | `herdr-crew:latest`, mutable tag — **exact digest unavailable** |
| Observed inner server | v0.9.0 |
| Upstream stable release | v0.9.3, published 2026-09-29 (a dated observation, not an installed version) |

The template digest was not recorded in that inventory. A mutable tag without a
recorded digest cannot prove which image a sandbox booted. Record one the next
time the template is built.

This table is the 2026-10-03 host inventory; it is not a live registry query.
The development sandbox inspected on 2026-10-04 still reports inner herdr 0.9.0
and uses the old kit protocol without run.json/qc.json. Fresh probes confirmed
the candidate model pins on its installed Claude Code 2.1.289, Codex 0.153.4 and
pi 0.85.1 CLIs. Earlier crew work used the old bootstrap defaults. No host driver,
kit or template was installed or published by these source changes.

### Candidate kit and driver pairing

Published kit and image 0.5.0 are immutable and defective: the image's Node.js
20.19.4 is below Claude Code 2.1.289 (>=22) and pi 0.85.1 (>=22.19), and `crew-check`
hid failed or blank version output. A fresh Codex 0.153.4 start in the task workspace
also stops at the trust dialog. The source kit is therefore the corrected candidate
0.5.1: it pins Node.js 22.22.1 (checksum-verified from nodejs.org), makes `crew-check`
truthful and enforces the Node floors, and trusts only the task workspace in Codex.
The 2026-10-03 inventory records published kit 0.4.5. The candidate adds a models
file, pinned starts and response probes, run acknowledgments, native phase recovery
and schema-v2 QC.
An old kit lacks these files; the candidate host refuses it. An old host cannot
supply the new run/ACK protocol and may still trust a prose DONE result.

The host has since published the corrected linux/arm64 image as
`docker.io/olegselajev241/herdr-crew:0.5.1` (digest
`sha256:45787cc3a320ee7677a1ca248beff29093d78748a30bbf6b7f7a4bae9eca230f`). It was built from the
independently reviewed Node component at commit `616d9593`. The only later change
under `kits/herdr-crew/template/` is comment text in `crew-check`; the Dockerfile
and every non-comment line are identical. The candidate kit is not published, and
no `latest` tag or installed default has changed.

Publishing the candidate kit and promoting it are separate host steps. Promotion
requires a tested driver/kit pair and waits for real sandbox acceptance, which is
still pending. The host must confirm the kit tag 0.5.1 is unused before publishing
it. Do not overwrite 0.4.5, 0.5.0 or any other already-published version tag.
Publish a candidate tag for testing, then
use `HERDR_TASK_KIT` to select that exact reference with the new driver. Only
after end-to-end checks pass should the host drain old tasks, move `:latest` and
install the new driver together. None of those promotion steps has happened, and nothing in this development sandbox built, published or promoted anything.

The [kit README](../kits/herdr-crew/README.md) explains model overrides,
`--report-only`, native recovery and ordinary `task goal` resume after a resource
grant. There is no `--continue` flag. Each allowed goal delivery creates fresh
run metadata; a valid change-scope QC report can accept HEAD equal to the new base.

### Next template build

`kits/herdr-crew/template/Dockerfile` now pins `ARG HERDR_VERSION=0.9.3`
instead of `latest`, so a template rebuild gets a known inner Herdr rather than
whatever is newest that day. This is the *inner* Herdr — an ordinary upstream
release, which does not need the fork's task commands.

The Dockerfile also pins the three npm CLI build defaults:
`CLAUDE_CODE_VERSION=2.1.289`, `CODEX_VERSION=0.153.4` and `PI_VERSION=0.85.1`.
These and `HERDR_VERSION=0.9.3` fix those version inputs. The base image tag and
apt packages remain mutable, so builds on different days can still differ;
these pins do not make the whole template reproducible.

The pin takes effect only on a host-performed template rebuild and publication.
Moving the observed sandbox runtime from 0.9.0 to 0.9.3 requires that rebuild and
a kit pointing to the chosen image. Record its digest and CLI versions. A kit
publication updates role packs and defaults for new sandboxes; it cannot replace
binaries in an already-created sandbox.

## Promoting a new host task driver

Never run `herdr update` on the host task driver, and never install an upstream
release binary over it. Upstream's updater downloads from `herdr.dev` and
renames the result over `env::current_exe()`. The downloaded binary is an
upstream build, so it has no `herdr task` at all — the driver would disappear
and you would find out the next time you tried to use it.

The fork refuses this in code (see the updater deviation below), but the refusal
is a safety net, not the procedure. To promote a new driver:

1. Start from a fork commit that passed independent QC.
2. Build the binary from that exact commit. On a clean Zig cache, run
   `scripts/fetch-zig-deps.sh` first — see deviation 6.
3. Record the artifact's checksum against that commit.
4. Verify it. These are three different checks and `just fork-compat-test`
   is only the last of them:
   - `just ci 'all()'` — the behavioral contracts, including the completion
     evidence and update-policy tests, which are Rust tests in the normal
     suite.
   - `just fork-compat-test` — the sync fixtures and the parsed
     workflow/kit contracts.
   - A real task, driven by the new binary against the staged kit version, to
     prove `herdr task` itself works end to end. See
     `kits/herdr-crew/README.md`.
5. Install it deliberately, keeping the previous artifact so you can roll back.

## Deviations

### 1. `herdr task` crew commands

**What.** `src/tasks.rs`, `src/cli/task.rs`, the `task` subcommand tree in
`src/cli/spec.rs`, and the `kits/herdr-crew/` kit. Host-side lifecycle for
sandboxes that run an orchestrator, planner, implementer, and QC agent.

**Why.** The entire reason this fork exists.

**Remove when.** Upstream ships an equivalent with the same role ids and kit
contract. There is no sign of that.

**Evidence.** The role, rotation, argv, and quoting tests in `src/tasks.rs`
(`parse_roles_round_trips_and_enforces_qc_independence`,
`rotation_is_deterministic_and_never_lets_implementer_qc_itself`,
`remote_command_quotes_unsafe_arguments`), and the kit declaration contracts in
`scripts/fork-workflows.test.ts`.

### 2. Sandbox isolation and startup repairs

**What.** `sbx create` always gets `--skills off`. Readiness is a real
`herdr agent list` request rather than `herdr status server` text or a `pgrep`
guard. The inner server starts via `setsid` in its own session. The kit
supervises the server independently of an interactive attachment. The host
creates the first workspace before starting the orchestrator, and confirms the
goal prompt actually moved the agent rather than treating a successful write as
delivery.

**Why.** Each replaced something that looked fine and was not.
`herdr status server` exits 0 whether or not a server is running, so its exit
code proved nothing. A `pgrep -f 'herdr server'` guard can match the probe
process itself. Without `setsid`, SSH cleanup killed the server's process group.
A fresh headless server has no workspace, so the first role had nowhere to go.

**Remove when.** Upstream provides equivalent readiness and session handling.
The `--skills off` default is ours regardless: it is a policy about what the
sandbox may reach, not a bug workaround.

**Evidence.** `task_sandboxes_are_always_created_with_the_shared_skill_store_off`
and `the_default_kit_is_a_published_image_not_a_local_path` in `src/tasks.rs`;
the kit's supervisor, network-policy, and published-image contracts in
`scripts/fork-workflows.test.ts`.

### 3. Narrow Codex updater suppression

**What.** The kit disables Codex's startup update check. For sandboxes created
from an older kit, the host dismisses that one menu, and only that one.

**Why.** An unattended crew cannot answer an interactive update prompt, and it
blocks startup. The recovery path is deliberately narrow: it matches on agent
kind, agent state, and the specific menu text together. Sending confirmation
keys to an arbitrary blocked agent would be answering permission dialogs on the
operator's behalf.

**Remove when.** No sandbox remains that was created from a kit older than
0.4.5 — the version that first disabled the check. The config setting stays;
only the screen-matching recovery goes.

**Evidence.** `codex_updater_recovery_rejects_history_and_unrelated_dialogs` in
`src/tasks.rs`, which asserts non-Codex agents and unrelated dialogs never
match.

### 4. Host task driver refuses to self-update

**What.** `self_update` and `auto_update` in `src/update.rs` return early when
the running build carries the task commands.

**Why.** Explained above. The check asks whether this build has `herdr task`,
not what the file is called, so renaming or copying the artifact does not get
around it. It says nothing about the inner sandbox Herdr, which keeps its own
updater.

**Remove when.** Upstream absorbs the task commands, so an upstream build is no
longer a downgrade.

**Evidence.** The fork policy tests in `src/update.rs`:
`manual_self_update_refuses_before_downloading_anything`,
`automatic_update_never_hands_off_a_replacement`,
`a_refused_update_leaves_the_running_artifact_untouched`, and
`the_task_cli_contract_survives_a_refused_update`.

### 5. Exact-commit completion evidence

**What.** The candidate `herdr task watch` exits 0 only after `RESULT: DONE` and
schema-v2 QC evidence agree on the current run and exact clean HEAD. Each goal
delivery mints run.json with the current base commit and scope. The report echoes
those fields, the goal digest and workspace, and names the review's start/finish
commit and round. Reported role kinds must match their workflow assignments.
This catches wiring mistakes; it does not authenticate the writer. All crew
processes share a user ID, so independence comes from the assignment and review
practice, not a cryptographic boundary between file writers.

**Why.** DONE is a claim the crew makes. Acceptance requires fresh evidence with
full 40-character commits, an unchanged clean review window and nonempty checks
that name their commands and all passed. No particular command is mandated:
QC must select checks appropriate to the actual goal.

Change scope permits the base commit or a descendant; report-only requires the
base itself. An already-completed implementation does not need a redundant commit
after a resource grant. Earlier blocked/failed attempts remain in `attempts` with
a nonblank reason and `superseded_by` pointing to a passing current check. Watch
prints those recovered attempts as history. A failed current check still refuses
acceptance. v1 evidence is refused with a regenerate-with-a-current-kit message.

**Remove when.** Never, unless completion stops meaning "reviewed".

**Evidence.** Rust fixtures in `src/tasks.rs` cover stale run/goal/workspace,
scope/base mismatch, review-window mismatch, HEAD moving after review, tracked
and untracked dirt, full commit IDs, v1 refusal, malformed evidence, configured
role mismatches, implementer==qc assignment rejection, empty/undescribed/failing
checks, recovered attempt links and history formatting. They accept both equal-base
and descendant change goals, reject unrelated history, and enforce report-only's
unchanged base. Metadata round-trips preserve v2 fields.

`crew_phase.py` complements the host gate with a run/round ledger. Python scenarios
in `scripts/test_crew_phase.py` cover native missed acknowledgment, wait-only
recovery, concurrent callers, stale rounds and exact-clean-HEAD reuse for QC.
Plan/implementation phases retain ancestor-or-equal reuse. The host's pure tests
also cover busy unfinished-goal refusal and exact current-run ACK matching. A
live isolated Gemini handoff exercised settlement and duplicate suppression on
inner herdr 0.9.0; this is not a published driver/kit end-to-end validation.

### 6. Zig dependency side-loading

**What.** `scripts/fetch-zig-deps.sh`.

**Why.** Zig's HTTP client gets `400 Bad Request` from
`deps.files.ghostty.org` where curl gets the same bytes fine, which breaks
`cargo build` at the vendored `zig build` step. Zig verifies packages by content
hash, so the transport does not matter: the script downloads with curl and
imports with `zig fetch`. The transport discrepancy was reproduced in the
2026-10-04 sandbox build. The helper now compares the computed hash with each
build.zig.zon declaration, tracks required versus lazy packages, and exits nonzero
when a required download, import or hash check fails. Optional failures are listed
explicitly; a successful prefetch does not prove every lazy package is available.
The actual `zig build` decides which lazy packages the requested target needs.

The same build needed packages from deps.files.ghostty.org and codeberg.org;
its lib-VT target did not need the optional fontconfig package from
gitlab.freedesktop.org. These are observed target requirements, not a grant for
all packages or domains. Behavioral shims in `scripts/test_fetch_zig_deps.py`
cover required/optional failures, hash mismatches, transitive discovery and the
GitHub .git-suffix rewrite.

**Remove when.** The target build's direct Zig downloads succeed and
`cargo build --locked` passes with fresh Zig global/local caches and no prefetch
or previously extracted package cache. Record the target, toolchain and network
conditions. A build that reused curl-imported packages does not meet this test;
the 2026-10-04 successful build used the helper, so it does not justify removal.

**Note.** The other half of the original build fix — installing the vendored
library into a per-target output directory so two targets sharing a checkout do
not overwrite each other's archive — **is now gone from the fork**. Upstream
`fff6c820` moved the Zig build into the `crates/ghostty-vt` workspace crate,
whose `build.rs` already installs into the per-build `OUT_DIR`. The fork hunk
was dropped in favour of upstream's.

### 7. Workflow publishing guards

**What.** Every inherited workflow that uses a secret or holds a write
permission is gated to `github.repository == 'herdrdev/herdr'`. Upstream already
guarded `preview`, `release`, and `pr-gate`; this fork adds the guard to
`website-deploy` and `label-next-release-issues`.

**Why.** Both fire on a push to master, which is exactly what an upstream merge
produces. `website-deploy` POSTs a deploy hook. `label-next-release-issues`
holds `issues: write` and closes issues referenced by `refs #N` lines in merged
upstream commit bodies. In this fork the secrets are absent, so today they fail
rather than act — but a missing guard is one configured secret away from acting
on someone else's project.

**Remove when.** Never while this fork exists.

**Evidence.** `scripts/fork-workflows.test.ts`, which parses the workflows and
asserts the guard on each publishing job *individually* — dropping one while the
others remain still fails — and *also* fails on any job that acquires a secret
or a write permission without a guard. That second check is the one that catches
a new publishing workflow arriving in a future upstream merge.

### 8. Daily upstream sync

**What.** `.github/workflows/fork-sync.yml` and `scripts/fork_sync.py`. Daily,
plus manual dispatch. Merges upstream into one rolling branch,
`automation/upstream-sync`, and opens or updates a single pull request against
fork master.

**Why.** It merges and never rewrites: no rebase, no squash, no force-push, no
reset of fork master. A rebase would drop fork commits, and a fork whose history
has been rewritten cannot be reviewed against what was tested. Conflicts fail
the run visibly rather than producing a partial result.

Two details worth knowing. The workflow runs the checks *itself* — a pull
request opened with the default `GITHUB_TOKEN` does not trigger the normal
`pull_request` workflows, so relying on downstream CI would mean publishing a
branch nobody checked. And the push goes through `fork_sync.publish()`, which
re-verifies the remote identity, that HEAD is still the commit that was
validated, and that the tree is clean. A raw `git push` in the workflow would
skip all three.

Nothing merges or deploys automatically. The pull request is for a human.

**Remove when.** Never while this fork exists.

**Evidence.** `scripts/test_fork_sync.py` builds real Git repositories and
covers first sync, idempotent no-ops, new upstream commits, fork master
advancing under an open pull request, branch reuse, a landed pull request,
conflicts, and refusal to publish after drift or into a non-fork remote.
`scripts/fork-workflows.test.ts` checks that the workflow uses that guarded path
rather than its own push, that scratch files stay out of the validated
checkout, and that publishing is gated on validation having succeeded.

## Keeping this honest

`just fork-compat-test` runs the model/phase helper scenarios,
`scripts/test_fork_sync.py` and `scripts/fork-workflows.test.ts`, and `just ci` runs it, so a merge that removes
one of these fails before it lands. The behavioral contracts — role ids, sandbox
argv, completion evidence, update policy — are Rust tests in the normal suite. Linux CI runs the
full test suite including the `live_handoff` integration test; macOS excludes
only `live_handoff`, matching upstream.

When upstream makes a deviation unnecessary, delete the deviation, its tests,
and its entry here in the same change.
