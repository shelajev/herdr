# Fork compatibility inventory

`shelajev/herdr` is a custom fork of `herdrdev/herdr`. It exists to add one
thing upstream does not have: `herdr task`, which runs a crew of coding agents
inside a Docker Sandbox. Everything in this file is a deviation from upstream
that has to survive the next merge, together with the condition that would let
us delete it.

Tested at source commit `5da0a01e1eedda054db0c81dd3a780000c40d9f0` merged into
the fork — that is upstream's master as of this sync, not a cherry-pick.

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
| Kit version in this source tree | 0.5.0 — **not published** |
| Published kit | `herdr-crew-kit:latest` = version 0.4.5 |
| Published kit digest (`:latest`) | `sha256:dbeb8b7c4ba2441d3f21d61430e9b5595fdc10f42ef95071d0a0890121de3afe` |
| Published kit digest (`:0.4.5`) | `sha256:4b28f8d50e028d6bbb8407a14dc1a7d0f549ce236338d8b2fd7dd5afe3594753` |
| Template image | `herdr-crew:latest`, mutable tag — **exact digest unavailable** |
| Observed inner server | v0.9.0 |
| Upstream stable release | v0.9.3, published 2026-09-29 (a dated observation, not an installed version) |

The template digest is genuinely unknown, not omitted. A mutable tag without a
recorded digest cannot prove which image a sandbox booted. Record one the next
time the template is built.

Nothing in that table has been upgraded by this work. The host driver is still
the 0.8.2 build, the cached template image is unchanged, and the published kit
is still 0.4.5. This sync produced commits; it did not install anything.

### The kit version gap

This source tree declares kit 0.5.0 because the completion protocol changed: QC
now writes a machine-readable report, and `herdr task watch` will not accept a
run without one. The published kit is still 0.4.5.

That gap is deliberate and must not be closed by overwriting the 0.4.5 tag.
It also cannot be closed one half at a time: **neither mismatched pair is
safe.**

| Host driver | Kit | Outcome |
|---|---|---|
| 0.8.2 (installed) | 0.4.5 (published) | Works. Today's arrangement. |
| new (this source) | 0.4.5 | Every task refused: 0.4.5 never writes `qc.json`. |
| 0.8.2 | 0.5.0 | Broken review step. The 0.5.0 QC brief tells the reviewer to copy a run id out of `run.json`, which only a new driver writes, so QC cannot produce a valid report — and the 0.8.2 driver accepts `RESULT: DONE` regardless. You get a pass with no gate behind it. |
| new | 0.5.0 | The intended pair. |

An earlier draft of this file claimed publishing the kit first was harmless.
That was wrong, for the reason in row three.

Promotion is therefore coordinated:

1. Let the tasks already running drain. A sandbox keeps the kit it was created
   from for its whole life, and a new driver refuses all 0.4.5 sandboxes.
2. Publish kit 0.5.0 under its own version tag, leaving `:0.4.5` alone.
3. Re-point `herdr-crew-kit:latest` to 0.5.0 and install the new host driver
   together, before dispatching any new task.

Tasks always run from a published kit; there is no local-path or ad-hoc
`--kit` route. To test the new pair, publish 0.5.0 under its own version tag
(nothing defaults to it yet) and point `HERDR_TASK_KIT` at that exact version
before step 3 moves `:latest`. See `kits/herdr-crew/README.md`.

### Next template build

`kits/herdr-crew/template/Dockerfile` now pins `ARG HERDR_VERSION=0.9.3`
instead of `latest`, so a template rebuild gets a known inner Herdr rather than
whatever is newest that day. This is the *inner* Herdr — an ordinary upstream
release, which does not need the fork's task commands.

That pin fixes one input, not the image. The base image tag, the agent CLI
versions installed from npm (`CLAUDE_CODE_VERSION`, `CODEX_VERSION`,
`PI_VERSION` all still default to `latest`), and apt packages are all still
mutable, so two builds of this Dockerfile on different days can differ. The
template is not reproducible; it is merely no longer silently changing which
Herdr it contains.

The pin takes effect on the next template build. The cached image still has
whatever `latest` resolved to when it was last built.

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

**What.** `herdr task watch` exits 0 only when the crew's QC agent has written
`/home/agent/crew/qc.json` naming the current run and the exact current commit.
`RESULT: DONE` alone no longer finishes a task. `FAILED` and `BLOCKED` are
unchanged — they are not claims of success.

**Why.** `RESULT: DONE` is prose an agent wrote about itself. The gate requires
the report to match the run the host minted, to name a full 40-character commit
that is still HEAD with a clean tree, to come from the configured QC role rather
than the implementer, and to list checks that actually ran and actually passed.

The gate is deliberately generic. The driver runs whatever repository and goal
you give it, including non-coding goals, so it does not require any particular
build or test command. What it will not accept is a PASS with no checks, a PASS
alongside a failed or skipped check, or a check entry that names nothing.

A sandbox created before this protocol has no `qc.json`. Those runs are refused
with a message saying so, rather than accepted or left hanging.

**Remove when.** Never, unless completion stops meaning "reviewed".

**Evidence.** The evidence tests in `src/tasks.rs` cover stale runs, wrong goals
and workspaces, abbreviated and uppercase commit ids, dirty trees, unsupported
schema versions, malformed reports, self-review, blank check entries, and a
commit arriving after review.

### 6. Zig dependency side-loading

**What.** `scripts/fetch-zig-deps.sh`.

**Why.** Zig's HTTP client gets `400 Bad Request` from
`deps.files.ghostty.org` where curl gets the same bytes fine, which breaks
`cargo build` at the vendored `zig build` step. Zig verifies packages by content
hash, so the transport does not matter: the script downloads with curl and
imports with `zig fetch`. This was reproduced again during this sync.

**Remove when.** `cargo build --locked` succeeds on a clean Zig cache without
running it.

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

`just fork-compat-test` runs `scripts/test_fork_sync.py` and
`scripts/fork-workflows.test.ts`, and `just ci` runs it, so a merge that removes
one of these fails before it lands. The behavioral contracts — role ids, sandbox
argv, completion evidence, update policy — are Rust tests in the normal suite. Linux CI runs the
full test suite including the `live_handoff` integration test; macOS excludes
only `live_handoff`, matching upstream.

When upstream makes a deviation unnecessary, delete the deviation, its tests,
and its entry here in the same change.
