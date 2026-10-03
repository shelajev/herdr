# herdr-crew: task crews in Docker Sandboxes

One task = one Docker SBX sandbox running four coding agents: an
**orchestrator that holds the goal**, plus a planner, an implementer, and
quality control. The goal-seeking intelligence is deliberately *inside* the
sandbox — the entity with "achieve X, be creative" incentives is the one that
must be contained. The host herdr only kicks tasks off, delivers goals,
watches status, and arbitrates resources; a host leader agent (codex) can do
those things conversationally, but it never holds a goal itself.

QC is **never the same model as the implementer**: rotation assigns each of
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

Four separate things are called "herdr" or "the kit" in this document, and they
are versioned and deployed independently:

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
  `workdir`, `goal.md`, `plan.md`, `qc-log.md` (`VERDICT:` lines, binding on
  the orchestrator), `status.md` (progress + final `RESULT:` line, parsed by
  the host), `escalations.md` (resource requests).

## Protocols

- **Completion**: the orchestrator appends `RESULT: DONE | FAILED | BLOCKED`
  to `status.md`; `herdr task watch` tails status/escalations and exits 1 on
  `FAILED` and 3 on `BLOCKED`.

  `DONE` is different: from kit 0.5.0 on it only *opens* the acceptance check.
  When you deliver a goal with `herdr task goal <slug> ...`, the host writes
  `/home/agent/crew/run.json` — a fresh run id, the goal's digest, and the
  workspace path — before it prompts the orchestrator, and discards any report
  left over from a previous run. QC must then write
  `/home/agent/crew/qc.json` after it has actually run its checks:

  ```json
  {
    "version": 1,
    "run_id": "<copied from run.json>",
    "goal_digest": "<copied from run.json>",
    "workspace": "<copied from run.json>",
    "commit": "<full 40-character HEAD that qc reviewed>",
    "verdict": "PASS",
    "implementer": "<implementer kind from crew/assignment>",
    "qc": "<qc kind from crew/assignment>",
    "checks": [
      {"name": "tests", "command": "just ci-tests 'all()'",
       "outcome": "passed", "exit_code": 0}
    ]
  }
  ```

  `herdr task watch` exits 0 only when all of this holds: the report is schema
  version 1; its `run_id`, `goal_digest`, and `workspace` match the current
  `run.json`; `commit` is a full 40-character id that is still the workspace
  HEAD with no uncommitted tracked changes; `implementer` and `qc` match the
  configured assignment and differ from each other; and `checks` is non-empty
  with every entry naming what ran and reporting success. Anything else exits 4
  and says why.

  Which checks are appropriate is QC's judgement — the host does not require
  any particular command, because a task sandbox may be driven at any
  repository or a non-coding goal. What it refuses is a PASS with no checks, a
  PASS next to a failed or skipped check, or a check entry that names nothing.

  A sandbox created from kit 0.4.5 or earlier never writes `qc.json`. A 0.5.0
  host driver refuses those runs with a message saying the sandbox predates the
  protocol, rather than accepting or hanging.
- **Escalation**: any crew member hitting a blocked domain records it in
  `escalations.md`; the watch surfaces it; the host (you or the leader codex)
  decides: `herdr task policy <slug> --allow <domain>`, then resume with
  `herdr task goal <slug> "resources updated, continue"`.

## One-time host setup

```bash
sbx setup ssh                                  # every sandbox becomes <name>.sbx
sbx secret set gemini -t "$GEMINI_API_KEY"
sbx secret set anthropic -t "$ANTHROPIC_API_KEY"
sbx secret set openai -t "$OPENAI_API_KEY"
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
own checklist — see `docs/fork-compatibility.md`. Never do it with
`herdr update`: that downloads an *upstream* build, which has no `herdr task`
at all.

### Kit 0.5.0 is not published yet

This source tree is kit version **0.5.0**. The published kit is still
**0.4.5** (`docker.io/olegselajev241/herdr-crew-kit:latest`, also `:0.4.5`),
which is what the CLI defaults to.

0.5.0 changes the completion protocol: its QC brief tells the reviewer to write
`/home/agent/crew/qc.json`, and a host driver built from this tree refuses a
run that has no such report. Those two halves have to move together.

**Neither mismatched pair works, so do not run one:**

| Host driver | Kit | What happens |
|---|---|---|
| 0.8.2 (installed) | 0.4.5 (published) | Works. The protocol in use today. |
| new (this tree) | 0.4.5 | Every task is refused. 0.4.5 never writes `qc.json`. |
| 0.8.2 | 0.5.0 | QC is told to copy a run id out of `run.json`, which only a new driver writes. QC cannot produce a valid report; the old driver accepts `RESULT: DONE` anyway, so you get a pass with a broken review step and no gate. |
| new | 0.5.0 | The intended pair. |

So promotion is coordinated, not incremental:

1. Let the tasks already running drain. They keep the kit they were created
   from for their whole life, and a new driver will refuse all of them.
2. Publish kit 0.5.0 under its own version tag. Do not overwrite `:0.4.5`.
3. Move the default (`:latest`) and install the new host driver together,
   before dispatching any new task.

Editing files under `kits/herdr-crew/` in this tree changes nothing about a
newly created sandbox; only a republish does.

**Testing the pair.** Tasks only ever run from a published kit — there is no
local-path or ad-hoc `--kit` route, by design: a sandbox must be reproducible
from a reference anyone can pull. So test by publishing 0.5.0 under its own
version tag first, which is safe because nothing defaults to it yet:

```bash
sbx kit push ./kits/herdr-crew docker.io/olegselajev241/herdr-crew-kit:0.5.0
```

Then run a task with the new driver against that exact version:

```bash
export HERDR_TASK_KIT=docker.io/olegselajev241/herdr-crew-kit:0.5.0
```

Only `herdr task new` reads it: the kit is resolved once, at creation. `goal`,
`watch`, and the rest talk to a sandbox that already exists.

Only once that pair is proven do you move `:latest` to 0.5.0 and install the
new host driver, in that order and with no tasks in flight.

Iterating on the kit means pushing a new version tag and pointing
`HERDR_TASK_KIT` at it — never `:latest`, which is what everything else
defaults to.

## Per task

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
# Ctrl-C detaches, and the crew keeps working. --no-watch returns immediately.
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

## Model notes

- The Google-models member is pi (pi.dev): it reads `GEMINI_API_KEY`, defaults
  to the google provider, and the driver pins `--model gemini-3.8-flash` (kit
  arg `gemini_model`; pi 0.85+ ships the model in its catalog).
- Rotation is deterministic from the slug (18 assignments: 6 crew
  permutations x 3 orchestrator choices), reproducible per slug, varied
  across slugs to balance subscription usage.

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
- The inner herdr is the latest upstream release binary; the orchestrator and
  the host driver depend on its CLI shapes.
- Template image is linux/arm64 only; API-key auth only until the
  subscription-OAuth kit blocks are verified.
- `RESULT: FAILED` and `RESULT: BLOCKED`, and the human-readable `VERDICT:`
  lines in `qc-log.md`, are still convention enforced by prompts rather than
  code. They are not claims of success, so a misreport costs you a re-run
  rather than a false pass. `RESULT: DONE` is no longer in that category: it is
  checked against `qc.json` as described under Protocols. `task attach` and the
  qc log remain the audit trail for everything else.
