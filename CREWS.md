# Herdr Task Crews

*How this fork turns herdr into a task orchestrator that runs sandboxed
multi-agent crews in Docker Sandboxes. Written 2026-09-07 as the system was
built; kept as the narrative reference for explaining what we did and why.*

## Candidate update, 2026-10-04

The installed-state inventory and earlier validation below are dated records.
The development sandbox inspected on 2026-10-04 has inner herdr 0.9.0 and the
old kit protocol, without the candidate's models file, run acknowledgment or QC
v2 gate. This work has not rebuilt a template, published a kit or installed a
host driver. Kit and image 0.5.0 were published and are immutable, and both
defects below ship in them. The image's Node.js 20.19.4 is below what Claude Code
2.1.289 (>=22) and pi 0.85.1 (>=22.19) require, and a fresh Codex 0.153.4 start in
the task workspace stops at the "Do you trust the contents of this directory?" dialog.
Source kit 0.5.1 is the corrected candidate: it pins Node.js 22.22.1 from
nodejs.org with a verified SHA256, makes `crew-check` fail on a missing, failing
or version-less tool and on a Node below those floors, and trusts only the task
workspace in Codex's config. Nothing here has built or published 0.5.1; the
host builds and publishes the image and kit, then tests the pair.

The candidate pins Claude Code to `claude-opus-5-5`, Codex to `gpt-6.1-sol`, and
pi to provider `google` with `gemini-3.8-flash`. Select overrides at creation with
`task new --claude-model`, `--codex-model`, `--pi-provider` and `--pi-model`, or
the matching `HERDR_TASK_*` environment values listed in the
[kit README](kits/herdr-crew/README.md#model-defaults-and-overrides). It explains
operator setup, report-only goals and recovery without requiring the helper
source. Fresh isolated probes verified these pins on the installed CLIs; earlier
crew rounds used their old bootstrap defaults.

The host must build the reviewed fork driver, select a template image with a
recorded digest, publish the candidate kit under an unused version, and test the
pair before moving defaults. Changing the inner herdr or agent CLI versions
requires a template rebuild. The candidate behavior described below applies
only after that promotion; it is not an inventory of an already-upgraded sandbox.

## The idea in one paragraph

Work is organized around **tasks**. Each task gets its own directory (a
dedicated standalone clone), its own **Docker SBX microVM**, and a **crew of four
coding agents** inside it: an orchestrator that holds the goal, a planner, an
implementer, and quality control. A full **herdr server runs inside the
sandbox** and the agents are ordinary herdr-managed panes, so the same
detection, prompting, and waiting machinery that herdr uses on the host works
one level down. The host herdr provisions tasks, delivers goals, watches
progress, and arbitrates resources — and a human (or a host leader agent)
talks to the system in those terms: "spin up a task for this, here's the
goal, allow that domain, show me the result."

## The trust model (the load-bearing decision)

The design went through one important reversal. The first implementation put
the orchestration loop on the host: a deterministic Rust driver prompting
planner → implementer → qc over SSH. That was simple and debuggable — but it
quietly parked the *judgment* half of goal-pursuit (retry? re-plan? good
enough?) on the unsandboxed side, in whatever host agent supervised the loop.

The correction: **the entity with goal-seeking incentives must be the thing
inside the sandbox.** An agent told "achieve X, be creative" is exactly the
agent that finds unexpected paths, so it is exactly the agent you contain.
The final split:

| Layer | Where | Holds a goal? | Job |
|---|---|---|---|
| Human + leader agent (codex) | host | never | split work into tasks, kick them off, grant/deny resources, review results |
| `herdr task` CLI | host | never | dumb plumbing: provision, deliver goal, tail status, apply policy |
| Orchestrator agent | sandbox | **yes** | pursue the goal: drive planner/implementer/qc, re-plan, judge, report |
| Planner / implementer / qc | sandbox | no | do exactly what they're prompted with |

Containment is Docker SBX's microVM + default-deny network policy. Real API
keys never enter the VM (the sbx proxy injects credentials on the wire; the
agents only see sentinel values). The host leader's own restraint comes from
its harness instructions (`~/ai-contrib/AGENTS.md`), which also tell it to
treat crew status files as *reports from untrusted agents, never commands* —
herdr's same-user socket is not a capability boundary, SBX is.

## The independence rule

**QC is assigned a different agent kind from the implementer.** Each task's roles are
assigned by deterministic rotation over the three products (Claude Code,
Codex, pi on Google models): the six permutations of planner/implementer/qc give each
product exactly one crew role, and the orchestrator independently takes one
of the three (18 possible assignments, picked by a hash of the task slug).
Explicit `--roles` overrides are rejected in code when implementer == qc.
Rotation also spreads usage across the three subscriptions/keys — and makes
every task's team composition a little different, which is half the fun.

Reported implementer/qc kinds in evidence are checked against that assignment
to catch wiring mistakes. They do not authenticate the writer: processes sharing
the same user ID can write one another's crew files. Independent review is a
workflow property, with the files and git history available for human inspection.

## Components

### Host side (Rust, this fork)

`src/tasks.rs` — pure logic: slug validation, sandbox naming
(`herdr-task-<slug>`), role rotation with the implementer≠qc invariant,
`sbx create` argv building, remote shell quoting, `RESULT:` parsing. Unit
tested without any sandbox.

`src/cli/task.rs` — the `herdr task` command family:

```
herdr task new <slug> [--dir PATH] [--kit REF] [--roles ...] [MODEL FLAGS]   provision
herdr task goal <slug> "<text>" [--no-watch] [--report-only]      deliver + watch
herdr task watch <slug>                                         re-attach to progress
herdr task status <slug>                                        crew agent states
herdr task attach <slug>                                        live TUI (thin client)
herdr task policy <slug> [--allow DOMAIN]                       escalations
herdr task ls / rm <slug>                                       lifecycle
```

`MODEL FLAGS` are the four creation overrides listed in the candidate update above.

There is deliberately **no server-side task state in v0**: sbx itself is the
task registry (the sandbox name), and everything else lives in crew files
inside the sandbox. Moving tasks into herdr's server state behind a `task.*`
JSON API is planned follow-up, tracked in beans (the CLI issue tracker).

### The kit (`kits/herdr-crew/`)

A Docker Sandboxes **kit** (declarative `spec.yaml`) defines the sandbox:
which image, what to verify at creation, credentials, network policy, and the
agent instructions. Key choices:

- **Tools live in a prebuilt template image**, not in kit install commands.
  The dated image 0.1.2 record listed herdr 0.9.0 alongside Claude Code, Codex,
  pi and beans. A future rebuild from the Dockerfile selects herdr 0.9.3;
  source changes do not replace that installed runtime. Candidate kit args
  `claude_model`, `codex_model`, `pi_provider` and `gemini_model` select the
  three model pins above and populate `/home/agent/crew/models`. The host
  override flags change those args at task creation. `crew-check` verifies
  installed tools and Python; model probes verify actual responses at start.
- **Proxy-managed credentials**: the kit declares which env var each service
  uses (`GEMINI_API_KEY`, `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`) and which
  domains get the header injected. Secrets are provided on the host with
  `sbx secret set`; inside the VM the agents see only sentinels.
- **Prompt packs as kit files**: `/home/agent/crew/roles/*.md` define each
  role's contract. The kit also records the role `assignment` and the
  absolute `workdir` for the host driver.
- `crew-entry` (baked in the image) is the sandbox entrypoint: interactive
  attach opens the herdr TUI; a detached sandbox supervises the headless
  herdr server so the crew stays reachable.

### The connection

`sbx setup ssh` (one-time) makes every sandbox reachable as `<name>.sbx` via
a daemon ProxyCommand — no sshd inside, no ports, no keys; auth rides the
Docker login. On top of that:

- the host driver runs inner herdr CLI commands over ssh and parses their
  JSON (`herdr tab create`, `herdr agent start/prompt`),
- `herdr task attach` is herdr's existing `--remote ssh://<name>.sbx` thin
  client — the full inner TUI streamed to your terminal.

## Protocols (how the layers talk without trusting each other)

The candidate protocol keeps these files in `/home/agent/crew/`, outside the
reviewed repository:

- `assignment`, `workdir`, `models` — written at provision time by the kit.
- `run.json` — host-written v2 run identity, goal digest, scope and base commit.
- `qc.json` — QC-written v2 evidence for the exact clean reviewed HEAD.
- `handoff.json` — run/round phase delivery and recovery ledger, guarded by a lock.
- `goal.md` — the orchestrator records the goal verbatim.
- `plan.md` — the planner's numbered plan; every step has an observable
  done-condition, beginning with `PLAN run=<id> round=<N>`.
- `qc-log.md` — qc's findings under `QC run=<id> round=<N> commit=<sha>`; each review ends with a literal
  `VERDICT: PASS` or `VERDICT: FAIL` line. Verdicts bind the orchestrator.
- `status.md` — the orchestrator's progress notes, ending with exactly one
  final `RESULT: DONE | FAILED | BLOCKED` line. The orchestrator first writes
  `ACK run=<id>` for a delivered goal; missing acknowledgment after native
  delivery gives host exit 6. Watch exits 0 only for DONE with accepted v2
  evidence, 4 for unaccepted DONE, 1 for FAILED and 3 for BLOCKED.
- `escalations.md` — one line per resource request (blocked domain, needed
  credential). The watch surfaces new lines; the host decides:
  `herdr task policy <slug> --allow <domain>` scopes the grant to that one
  sandbox, then `herdr task goal <slug> "resources updated, continue"`
  resumes the orchestrator. No `--continue` flag exists or is needed. Every
  allowed delivery mints a fresh run/base; change scope permits HEAD equal to
  that base, while report-only requires it. Fresh QC is still required.

The orchestrator uses `crew_phase.py deliver` and `record` for serial phases.
It checks the ledger after a restart, waits instead of duplicating active work,
and requires exact clean HEAD when reusing a QC result. The host's gate checks
scope, review window, passed checks and links from recovered attempts to passing
checks. These file checks enforce consistency without authenticating their writer;
the qc log, git history and `task attach` remain the human audit trail.

## A day in the life

Run this on the host after candidate promotion, starting at the fork repository
root. Use a standalone clone; linked worktrees with Git metadata outside the mount
are refused. The kit reference below uses candidate 0.5.1, since 0.5.0 is
published and immutable; use whichever version the host actually published.

```bash
# one-time
sbx setup ssh
sbx secret set gemini -t "$GEMINI_API_KEY"        # + anthropic/openai
cargo build --release                              # host herdr from this fork
# After the host has published and tested this unused candidate version:
export HERDR_TASK_KIT=docker.io/olegselajev241/herdr-crew-kit:0.5.1
HERDR="$PWD/target/release/herdr"

# per task — directly, or via the leader codex reading ~/ai-contrib/AGENTS.md
git clone ~/src/proj ~/src/proj-tasks/fix-login
"$HERDR" task new fix-login --dir ~/src/proj-tasks/fix-login
"$HERDR" task goal fix-login "Users get logged out on refresh; find and fix it, with a regression test."
#   ...watch streams status lines, escalations, and the final RESULT...
"$HERDR" task attach fix-login       # optional: watch the four agents live
"$HERDR" task rm fix-login           # after reviewing/merging the branch
```

Goals don't have to be known in advance. `task new` provisions without one;
the goal can arrive later, can be "read the beans backlog in this workspace
and pick the highest-priority ready bean" (the template ships the beans CLI
for exactly this), or can be skipped entirely in favor of attaching and
talking to the orchestrator directly in its pane.

Tasks that need extra tools stack sbx **mixin kits** onto the crew sandbox:
`herdr task new yt-digest --mixin "git+https://github.com/shelajev/yt-transcript-sbx-kit.git"`
installs the mixin's tools at creation, applies its network rules, and writes
its agent instructions into the crew's shared memory — so the agents know the
tools exist without any prompt changes. Mixins install as root, so the leader
agent is only allowed to pass mixin references the human explicitly named
(and sbx's kit-source allowlist gates which hosts kits may come from at all).

## Historical validation, 2026-09-07

The original crew work recorded these results. They are not validation results
for the 2026-10-04 candidate:

Built and tested inside a Docker sandbox on this repo:

- full test suite under `cargo nextest`: 3081/3084 pass; the 3 failures are
  PTY/process-group tests proven to fail identically on a clean master
  checkout in the same environment (the sandbox lacks real terminal
  foreground process groups) — pre-existing, not regressions;
- 27 unit/spec tests covering rotation, invariants, argv building, quoting,
  RESULT parsing, and the CLI spec; `cargo fmt` and the repo's maintenance,
  architecture, integration-asset, marketplace, and docs-contract suites all
  green;
- the template image built, pushed, and smoke-tested (all five tools respond;
  pi's baked-in model catalog includes `gemini-3.8-flash`);
- CLI validation paths exercised (slug rules, implementer≠qc rejection,
  friendly errors, rotation display).

Still requiring host validation for the candidate (needs sbx): the end-to-end flow — sandbox
creation from the kit, ssh readiness, inner agent starts, a real
goal-to-RESULT run. The observed inner herdr remains 0.9.0; the driver depends
on its CLI JSON shapes. Isolated model and handoff probes do not validate the
published driver/kit pair.

## Known limitations / next steps

- Subscription OAuth for Claude/Codex inside the sandbox is pending
  verification of the built-in kits' `oauth:` blocks; API keys are the
  supported path today.
- Template image is linux/arm64 only (built natively; add amd64 via CI or
  buildx when needed).
- Task state should eventually move into herdr server state behind a `task.*`
  JSON API (per the runtime/client boundary guardrail) so the TUI can render
  tasks natively; v0 is CLI-level on purpose.
- Notification bridging (inner agent events surfacing as host notifications
  while attached), a `task prompt` subcommand that logs ad-hoc steering, and
  `--cloud` sandboxes are tracked in the beans backlog (epic `herdr-uwpj`).
