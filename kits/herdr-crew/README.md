# herdr-crew: task crews in Docker Sandboxes

This page describes the **unpublished candidate** host driver and kit. Existing
sandboxes keep the kit and template they were created with. The dated inventory
and promotion steps below explain what must change before these instructions
apply to a newly created task.

One task = one Docker SBX sandbox running four coding agents: an
**orchestrator that holds the goal**, plus a planner, an implementer, and
quality control. The goal-seeking intelligence is deliberately *inside* the
sandbox — the entity with "achieve X, be creative" incentives is the one that
must be contained. The host herdr only kicks tasks off, delivers goals,
watches status, and arbitrates resources; a host leader agent (codex) can do
those things conversationally, but it never holds a goal itself.

QC is assigned a different agent kind from the implementer: rotation assigns each of
Claude Code, Codex, and pi (pi.dev, on Google models) exactly one of the planner/implementer/qc
roles per task (the orchestrator independently takes one of the three
products), and explicit `--roles` overrides are rejected when qc equals
implementer.

```
host (no goals here)                     sandbox: herdr-task-<slug>
─────────────────────────                ─────────────────────────────────────
herdr task new <slug>    ── sbx create ─▶ herdr-crew kit + template image
herdr task goal <slug>   ── ssh ────────▶ orchestrator  (holds the goal)
  then only watches status                  ├─▶ planner      writes plan.md
herdr task watch <slug>  ◀─ status.md ──    ├─▶ implementer  commits work
herdr task policy <slug> ◀─ escalations     └─▶ qc           VERDICT lines
  grant/deny resources                    orchestrator appends RESULT: DONE|
herdr task attach <slug> ── live view      FAILED|BLOCKED to status.md
```

## Components

The host driver, kit, template and inner herdr are versioned and deployed
independently.

| Layer | What it is | Where it comes from |
|---|---|---|
| **Host `herdr` binary** | The *fork* build, the only one with `herdr task`. Creates sandboxes, delivers goals over SSH, tails crew files, decides whether a run is accepted. | Built from this repository and installed deliberately |
| **The kit** | The sbx declaration of how a sandbox boots: image, credentials, network allowlist, role briefs, crew files. | Published image, `herdr-crew-kit` |
| **The template image** | The prebaked rootfs the kit boots, with the agent CLIs and an inner herdr already installed so creation is fast. | Published image, `herdr-crew` |
| **Inner herdr** | A stock *upstream* herdr running headless inside the sandbox. It has no task commands and needs none: it is the workspace/terminal manager the orchestrator uses to open panes and start, prompt, and watch the other agents. | An upstream release, baked into the template |

So there are two herdrs for two different jobs. The outer one manages
sandboxes and is a fork; the inner one manages panes and is upstream. Nothing
you change in this repository reaches the inner one — that happens when the
template is rebuilt.

- **`herdr task` CLI** (`src/cli/task.rs`, pure logic in `src/tasks.rs`):
  `new`, `goal`, `watch`, `ls`, `status`, `attach`, `policy`, `rm`. The host
  starts only the orchestrator and delivers the goal; the orchestrator
  assembles and drives the rest of the crew through the *inner* herdr CLI.
  Driving is one level deep: only the orchestrator starts, prompts, or waits on
  another agent. Planner, implementer, and qc never drive each other — each
  works on what it was prompted with and reports by writing its own crew file.
- **This kit** (`kits/herdr-crew/spec.yaml`): sandbox kit pointing at the template image;
  verifies tools at create time (`crew-check`), injects the role prompt packs
  (`files/home/crew/roles/`), records the role assignment and workspace path,
  declares proxy-managed credentials (real API keys never enter the VM) and
  the baseline network allowlist.
- **Template image** (`template/Dockerfile`, published as
  `docker.io/olegselajev241/herdr-crew:latest`, linux/arm64): herdr + Claude
  Code + Codex + pi + beans baked in so sandbox creation is fast; `crew-entry`
  supervises the headless herdr server.
- **Crew files** (`/home/agent/crew/` in the sandbox): `assignment`,
  `workdir`, `models`, `goal.md`, `plan.md`, `qc-log.md`, `status.md` and
  `escalations.md`. The host writes `run.json`; QC writes `qc.json`; the native
  phase helper records delivery/recovery in `handoff.json` under `handoff.lock`.
  These files live outside the reviewed repository.

## One-time host setup

Run these commands on the host from this fork's repository root. Install Docker
Sandboxes (`sbx`) and make its daemon available first. The build needs the Rust
version in rust-toolchain.toml and Zig 0.16.0 on PATH (or set `ZIG` to its binary).
The prefetch script also needs Python 3 and curl. Use existing provider API keys
in the secret commands; their values are never printed here.

```bash
sbx setup ssh                                  # every sandbox becomes <name>.sbx
sbx secret set gemini -t "$GEMINI_API_KEY"
sbx secret set anthropic -t "$ANTHROPIC_API_KEY"
sbx secret set openai -t "$OPENAI_API_KEY"
bash scripts/fetch-zig-deps.sh                  # required on a clean Zig cache here
cargo build --release                          # -> target/release/herdr

# Only if you plan to use git-hosted mixins: sbx's kit-source allowlist
# defaults to docker.io/ and rejects anything else.
sbx settings set kit.allowedSources '["docker.io/","github.com/shelajev/"]'
```

`cargo build` does not install anything. The `herdr` on your `$PATH` is still
whatever you installed before — an older driver, or an upstream build with no
`herdr task` at all — so every example below runs the freshly built binary by
path, as `$HERDR`:

```bash
HERDR="$PWD/target/release/herdr"
"$HERDR" task ls
```

Installing it as your host task driver is a deliberate promotion step with its
own checklist in [fork compatibility](../../docs/fork-compatibility.md). Never do it with
`herdr update`: that downloads an *upstream* build, which has no `herdr task`
at all.

### Dated installed-state inventory

This source tree is kit version **0.5.0**, not published yet. The host inventory
recorded on 2026-10-03 lists host task driver 0.8.2 and published kit 0.4.5
(`docker.io/olegselajev241/herdr-crew-kit:latest`, also `:0.4.5`). These are dated
observations, not a claim about what a mutable registry tag resolves to later.

The development sandbox inspected on 2026-10-04 runs inner herdr 0.9.0 with the
old role protocol: no run.json/qc.json and no automatic three-model preflight.
Its installed CLIs are Claude Code 2.1.289, Codex 0.153.4 and pi 0.85.1. Fresh
isolated probes verified the candidate pins on those CLIs. That does not mean
earlier crew work ran on those pins, or that a template/kit was rebuilt or
published. This source work has not installed a new host driver or released a kit.

### Candidate promotion requirements

The host owns these steps. Drain old tasks before switching the default driver
and kit; existing sandboxes keep their old files. Build the fork driver from the
reviewed commit and record its checksum. Do not install it over an upstream build
by running `herdr update`.

1. Confirm the intended kit tag is unused. Keep 0.5.0 only if it has never been
   published; otherwise bump the kit version and its README version string before
   publication. Never overwrite `:0.4.5` or reuse a published version tag.
2. Choose and record the template image digest. To move the observed inner runtime
   from 0.9.0 to the Dockerfile's 0.9.3 pin, the host must rebuild and publish a
   template, then point the kit's image at it. Likewise, changing installed CLI
   versions requires a template rebuild. The successful model probes alone do
   not require a CLI upgrade, and source edits do not alter a cached template.
3. Validate and publish the candidate kit under its own version tag. Test it with
   the new driver using `HERDR_TASK_KIT` set to that exact published reference.
   Model preflight, a change goal, a report-only goal, an override, a rejected model
   and recovery after a resource grant must work before promotion.
4. Only after that pair passes, move `:latest` and install the new driver together,
   with no old tasks in flight. Retain the previous host artifact for rollback.

A new driver refuses an old kit that lacks pins/helpers/evidence. An old driver
paired with the new kit cannot supply the required run/ACK protocol and may still
trust DONE without a gate. Use the tested pair together. Source compilation and
unit tests do not establish that the published pair works end to end.

Tasks run from published kits. For the host's later staging step, from this
repository root and only after confirming the version tag is unused:

```bash
sbx kit push ./kits/herdr-crew docker.io/olegselajev241/herdr-crew-kit:0.5.0
export HERDR_TASK_KIT=docker.io/olegselajev241/herdr-crew-kit:0.5.0
```

These are publication instructions, not actions performed by this change. If the
version had to be bumped, use that version in both commands. Only `task new`
resolves the kit; `goal` and `watch` use the already-created sandbox. Editing local
kit files does not update it. The host's detailed inventory and build checklist
are in [fork compatibility](../../docs/fork-compatibility.md).

## Per task

Use these commands only after the host has published the candidate kit. Until
`:latest` is promoted, export `HERDR_TASK_KIT` with that tested version reference
as shown above; otherwise `task new` resolves `:latest`, not local kit files.
Keep `$HERDR` set to the built fork driver. If `CARGO_TARGET_DIR` is set, use its
release binary path instead of `$PWD/target/release/herdr`.

`herdr task new` takes the current directory as the task's workspace unless
`--dir` says otherwise, so start from the project you want worked on.

```bash
cd ~/src/some-project
"$HERDR" task new fix-login               # workspace = $PWD; rotation picks the four roles
"$HERDR" task new yt-digest \
  --mixin "git+https://github.com/shelajev/yt-transcript-sbx-kit.git"
                                          # stack extra tools onto the crew sandbox;
                                          # the mixin's agent memory teaches the crew its tools
"$HERDR" task goal fix-login "Users get logged out on refresh; find and fix it, with a regression test."
# delivers the goal, then enters watch mode itself and blocks until the run ends;
# Ctrl-C detaches, and the crew keeps working. --no-watch returns after the run ACK.
"$HERDR" task watch fix-login             # re-attach to progress any time
"$HERDR" task attach fix-login            # live TUI view of the whole crew
"$HERDR" task rm fix-login
```

Task creation always passes `--skills off`: no host shared skill store is
mounted. Crew-installed skills stay in the sandbox unless explicitly installed
into the mounted project. Use a standalone clone: linked worktrees whose Git
metadata lives outside the mount are rejected.

The workspace directory mounts into the sandbox at the same absolute path;
commits land in it directly — use a dedicated standalone clone per task.

Mixins install as root inside the sandbox, so treat a mixin reference as code
you're choosing to run. The `--mixin` example above needs the
`kit.allowedSources` setting from the host setup section; without it sbx
rejects a git-hosted kit.

## Model defaults and overrides

The candidate kit starts every role with an explicit model, including the
orchestrator. Rotation selects agent kinds; the model for each kind comes from
the kit's arguments.

| Agent kind | Kit argument(s) | Default | Host creation flag | Host environment fallback |
|---|---|---|---|---|
| Claude Code (`claude`) | `claude_model` | `claude-opus-5-5` | `--claude-model` | `HERDR_TASK_CLAUDE_MODEL` |
| Codex (`codex`) | `codex_model` | `gpt-6.1-sol` | `--codex-model` | `HERDR_TASK_CODEX_MODEL` |
| pi (`pi`) | `pi_provider`, `gemini_model` | `google`, `gemini-3.8-flash` | `--pi-provider`, `--pi-model` | `HERDR_TASK_PI_PROVIDER`, `HERDR_TASK_PI_MODEL` |

On the host, choose an available model at task creation, for example:

```bash
"$HERDR" task new review-api --dir ~/src/api-review --claude-model claude-sonnet-5-5
HERDR_TASK_CODEX_MODEL=gpt-6.1-sol "$HERDR" task new fix-api --dir ~/src/api-fix
```

Flag values win over host environment values, which win over kit defaults. An
override affects every role using that kind. The driver passes it as the matching
`--kit-arg` at sandbox creation. Changing host environment values later does not
change an existing task.

The kit writes `/home/agent/crew/models`, the authoritative sandbox values:

```text
claude=claude-opus-5-5,codex=gpt-6.1-sol,pi=google/gemini-3.8-flash
```

It also sets convenience environment variables `HERDR_CREW_CLAUDE_MODEL`,
`HERDR_CREW_CODEX_MODEL`, `HERDR_CREW_PI_PROVIDER` and `HERDR_CREW_GEMINI_MODEL`.
The models file controls starts; editing only those environment variables does
not override it. To select other values as an operator, create a task with the
flags above. pi always receives both provider and model.

The model helper starts workers and probes a nonce-bound assistant response in
their session artifacts. The host uses the same pins and probes the orchestrator
when idle/done; a busy orchestrator gets a model-not-verified warning. A missing
models file/helper refuses an old kit. A model mismatch or unavailable model stops
the workflow visibly; there is no fallback to another model. Inspect the failure
through `task attach`; fix account access or create a task with an explicit valid
override. A native delivery miss is distinct from a provider rejection: the probe
can retry twice only while the same terminal is idle and no nonce was observed.
A model catalog entry alone is not evidence of account availability.

## Goal delivery and completion

Every allowed `herdr task goal` delivery creates a fresh `run.json` with schema
version 2: run id, goal digest, workspace, scope and `base_commit` (current Git
HEAD). The repository must already have a commit. The driver warns if the tree
is dirty; QC acceptance later requires a clean tree, including no untracked files.
A new goal is refused while the orchestrator is working on an unfinished run;
use `herdr task watch <slug>` to follow that work.

The native prompt names the run. The orchestrator must append `ACK run=<id>`
to `status.md` before delegating. The host waits up to 120 seconds for that exact
acknowledgment. Exit 6 means native delivery succeeded but the file acknowledgment
was missing or unreadable. Inspect with `herdr task attach <slug>` before sending
again. `--no-watch` returns after acknowledgment; it does not skip this check.

The default scope is `change`. QC may accept the base commit itself or a
descendant when the goal is already satisfied. A resume can begin at work that
was completed earlier; an extra commit solely to differ from the base is not
required. QC still has to inspect the current code and verify the goal.

For an investigation or validation that must leave the commit unchanged, use
`--report-only` on the host:

```bash
"$HERDR" task goal audit-login "Review the login flow and report findings; do not change code." --report-only
```

Create `audit-login` first with `"$HERDR" task new audit-login --dir ~/src/login-audit`,
using a dedicated standalone clone for that directory. Report-only
requires the reviewed HEAD to equal `base_commit`, a clean tree and passing
checks appropriate to the investigation. Findings belong in the crew files;
the implementer makes no commit.

Completion is serial: implementation stops, QC reviews round N, the phase helper
records that review, then the orchestrator writes `RESULT: DONE`. QC reads HEAD
and `git status --porcelain` before and after its checks. Both HEAD reads must
match and both status reads must be empty. Only QC writes `qc.json`:

```json
{
  "version": 2,
  "run_id": "<copied from run.json>",
  "goal_digest": "<copied from run.json>",
  "workspace": "<copied from run.json>",
  "scope": "change",
  "base_commit": "<copied from run.json>",
  "round": 1,
  "review": {
    "started_commit": "<full 40-character HEAD before checks>",
    "finished_commit": "<same HEAD after checks>"
  },
  "commit": "<same HEAD>",
  "verdict": "PASS",
  "implementer": "<assigned implementer kind>",
  "qc": "<assigned qc kind>",
  "attempts": [
    {"name": "initial tests", "command": "just ci-tests 'all()'",
     "outcome": "blocked", "exit_code": 127,
     "reason": "toolchain missing; installed before retry", "superseded_by": "tests"}
  ],
  "checks": [
    {"name": "tests", "command": "just ci-tests 'all()'", "outcome": "passed", "exit_code": 0}
  ]
}
```

The host requires scope and base to echo run.json, along with run id, goal digest
and workspace. The round is an integer at least 1. Review start, review finish,
reported commit and current HEAD must all be the same full 40-character commit
id, with a clean tree. Moving HEAD or editing files after review invalidates it.

`checks` must contain at least one described, passing check, each with a nonblank
name and command, outcome `passed` and exit code 0. Failed, blocked or skipped
current checks refuse acceptance. Optional `attempts` preserves earlier failures:
each has outcome `blocked` or `failed`, a nonblank reason, and `superseded_by`
naming a passing check. Its exit code is optional. An unrecovered attempt refuses
acceptance. Watch prints these as `history |` lines, separate from successful
checks. The example commands are illustrative; QC chooses checks for the actual
repository and goal.

Reported `implementer` and `qc` kinds must match the configured workflow
assignment and differ. This catches wiring mistakes. It does not authenticate
who wrote the file: agents sharing one user ID can write each other's files.
Independence is a property of the assignment, with QC's work available for human
inspection, rather than a cryptographic guarantee.

| Host outcome | Exit | Operator action |
|---|---|---|
| DONE with accepted v2 evidence | 0 | Review the committed work before merging it. |
| FAILED | 1 | Read the reason in status.md and inspect the task. |
| BLOCKED | 3 | Review escalations and grant only needed resources, then resume. |
| DONE without acceptable evidence | 4 | Inspect the reported mismatch and request a fresh QC round. |
| Native delivery succeeded, ACK missing/unreadable | 6 | Attach and inspect before another delivery. |

A v1 report is refused with a regenerate-with-a-current-kit message. Old sandboxes
without model pins/helpers or QC evidence must be recreated with the matching
candidate kit and driver; changing the host binary cannot upgrade their files.

## Stalls, resource grants and recovery

On the host, `task policy <slug>` without `--allow` prints the sandbox's
`escalations.md`. Read that request before granting a domain, then resume with
the ordinary goal command:

```bash
"$HERDR" task policy fix-login                 # read the recorded requests
"$HERDR" task policy fix-login --allow deps.files.ghostty.org
"$HERDR" task goal fix-login "resources updated, continue"
```

There is no `--continue` flag and none is needed. Each delivery mints a new run
and base at current HEAD, including after `RESULT: BLOCKED`. The crew inspects
existing commits before doing more work and obtains fresh QC for the new run.
If it is still working without a RESULT, watch it instead of issuing a parallel
goal. A domain grant does not repair provider authentication, quota or model
availability errors; resolve those through the host's credential/account setup.

Inside the sandbox, the orchestrator uses `crew_phase.py` to keep phase delivery
serial. The ledger identifies the run, round, role, phase and delivery sequence.
It survives an orchestrator restart, archives earlier runs and prevents duplicate
prompts for work already recorded. A timeout while the same terminal is working
causes the next delivery to wait. A lost or changed terminal triggers a RETRY
that tells the worker to inspect existing work first.

For manual recovery, first use `"$HERDR" task attach fix-login` to check that the
orchestrator is not already issuing work, then detach. Open a sandbox shell from
the host with `ssh herdr-task-fix-login.sbx` (enabled by `sbx setup ssh`). Inside
that shell, inspect `/home/agent/crew/handoff.json` for the current round and phase.
Write the worker's instructions to `/home/agent/crew/qc-prompt.txt` using your editor.
The example below uses round 1 and role qc; replace them with the actual round and
assigned role. Phases are `plan`, `implement`, `revise` and `qc`.

```bash
python3 /home/agent/crew/bin/crew_phase.py status --role qc --phase qc --round 1
python3 /home/agent/crew/bin/crew_phase.py deliver --role qc --phase qc --round 1 --text-file /home/agent/crew/qc-prompt.txt --timeout 1800000
python3 /home/agent/crew/bin/crew_phase.py record --phase qc --round 1
```

`status` reads the recovery decision without changing the ledger. `deliver`
sends or waits, checks terminal identity and a fresh native state sequence, then
records completion when phase files agree. `record` verifies those files again
and can finish recording after an interrupted caller. It never makes an unfinished
phase successful. Plan evidence names its run and round; implementation requires
a clean committed tree; QC evidence must name the matching round and commit.
A recorded QC FAIL sends work back for revision. A passed review is reusable only
at its exact clean HEAD; earlier plan/implementation phases can remain complete
when their commit is an ancestor of HEAD.

| Phase-helper exit | Meaning | Recovery |
|---|---|---|
| 0 | Settled or already complete | Inspect the result; a settled QC verdict can still be FAIL. |
| 20 | Native prompt stalled or acknowledgment missing | Inspect status/evidence, then retry the same phase explicitly; never assume it ran. |
| 21 | Agent blocked | Resolve the permission/resource issue before retrying. |
| 22 | Timeout | Retry the same delivery; it waits without prompting if that terminal is still working. |
| 23 | Agent not running | Restart only the absent role through the pinned model helper, then retry. |
| 24 | Agent already working without an owned phase | Investigate its work; do not add another prompt. |
| 25 | Another phase owns the run | Finish or resolve that phase first. |
| 26 | Evidence or native response cannot be verified | Inspect the named files/error and repair the evidence before recording. |

The orchestrator normally runs these commands. An operator recovering manually
must first attach and confirm it is not issuing the same work. Do not bypass the
helper with raw prompts, edit the ledger to force completion, or use idle-only
waits: a background agent can finish as `done`.

## Unattended startup

The host probes a real `herdr agent list` request and creates the first workspace
before starting the orchestrator. Its recovery server starts in a separate Linux
session so SSH cleanup cannot terminate the server process group. Kit startup
supervises the server independently of an interactive attachment. Codex startup
update checks are disabled; update crew CLI versions through template maintenance.
`task status` shows blocked-agent screens and server logs when diagnosis is needed.

## Known limitations (v0)

- Task state lives in sbx (sandbox name) and the sandbox's crew files; the
  host server keeps no task registry yet (planned: `task.*` JSON API).
- The inner herdr version is selected at template build time; the orchestrator
  and host driver depend on its CLI shapes. Recheck the pair after upgrades.
- Template image is linux/arm64 only; API-key auth only until the
  subscription-OAuth kit blocks are verified.
- `RESULT: FAILED` and `RESULT: BLOCKED`, and the human-readable `VERDICT:`
  lines in `qc-log.md`, are still convention enforced by prompts rather than
  code. They are not claims of success, so a misreport costs you a re-run
  rather than a false pass. `RESULT: DONE` is no longer in that category: it is
  checked against `qc.json` as described under Goal delivery and completion. `task attach` and the
  qc log remain the audit trail for everything else.
