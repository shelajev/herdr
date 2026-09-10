# Herdr Task Crews

*How this fork turns herdr into a task orchestrator that runs sandboxed
multi-agent crews in Docker Sandboxes. Written 2026-09-07 as the system was
built; kept as the narrative reference for explaining what we did and why.*

## The idea in one paragraph

Work is organized around **tasks**. Each task gets its own directory (a
worktree or clone), its own **Docker SBX microVM**, and a **crew of four
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

**QC is never the model that implemented the work.** Each task's roles are
assigned by deterministic rotation over the three products (Claude Code,
Codex, pi on Google models): the six permutations of planner/implementer/qc give each
product exactly one crew role, and the orchestrator independently takes one
of the three (18 possible assignments, picked by a hash of the task slug).
Explicit `--roles` overrides are rejected in code when implementer == qc.
Rotation also spreads usage across the three subscriptions/keys — and makes
every task's team composition a little different, which is half the fun.

## Components

### Host side (Rust, this fork)

`src/tasks.rs` — pure logic: slug validation, sandbox naming
(`herdr-task-<slug>`), role rotation with the implementer≠qc invariant,
`sbx create` argv building, remote shell quoting, `RESULT:` parsing. Unit
tested without any sandbox.

`src/cli/task.rs` — the `herdr task` command family:

```
herdr task new <slug> [--dir PATH] [--kit REF] [--roles ...]   provision
herdr task goal <slug> "<text>" [--no-watch]                    deliver + watch
herdr task watch <slug>                                         re-attach to progress
herdr task status <slug>                                        crew agent states
herdr task attach <slug>                                        live TUI (thin client)
herdr task policy <slug> [--allow DOMAIN]                       escalations
herdr task ls / rm <slug>                                       lifecycle
```

There is deliberately **no server-side task state in v0**: sbx itself is the
task registry (the sandbox name), and everything else lives in crew files
inside the sandbox. Moving tasks into herdr's server state behind a `task.*`
JSON API is planned follow-up, tracked in beans.

### The kit (`kits/herdr-crew/`)

A Docker Sandboxes **kit** (declarative `spec.yaml`) defines the sandbox:
which image, what to verify at creation, credentials, network policy, and the
agent instructions. Key choices:

- **Tools live in a prebuilt template image**, not in kit install commands.
  `docker.io/olegselajev241/herdr-crew` (linux/arm64) bakes in the latest
  released herdr (0.9.0 in image 0.1.2),
  Claude Code, Codex, pi (pi.dev — the Google-models member: provider google,
  `GEMINI_API_KEY`, model pinned to `gemini-3.8-flash` by the kit's
  `gemini_model` arg), and the beans issue tracker. Template
  images are cached by the sandbox runtime, so creating a task takes seconds;
  the kit's install step is just `crew-check`, which verifies the tools exist
  and fails fast on a wrong image.
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

Everything crosses the boundary as **files in `/home/agent/crew/`**, written
by the crew, read by the host:

- `assignment`, `workdir` — written at provision time by the kit.
- `goal.md` — the orchestrator records the goal verbatim.
- `plan.md` — the planner's numbered plan; every step has an observable
  done-condition.
- `qc-log.md` — qc's findings; each review ends with a literal
  `VERDICT: PASS` or `VERDICT: FAIL` line. Verdicts bind the orchestrator.
- `status.md` — the orchestrator's progress notes, ending with exactly one
  final `RESULT: DONE | FAILED | BLOCKED` line. `herdr task watch` tails this
  file and exits 0/1/3 accordingly. The pack forbids `DONE` unless qc's
  latest verdict is PASS.
- `escalations.md` — one line per resource request (blocked domain, needed
  credential). The watch surfaces new lines; the host decides:
  `herdr task policy <slug> --allow <domain>` scopes the grant to that one
  sandbox, then `herdr task goal <slug> "resources updated, continue"`
  resumes the orchestrator.

These are prompt-enforced conventions, not cryptographic ones — a misbehaving
orchestrator can misreport. The qc log, the git history in the workspace, and
`task attach` are the audit trail.

## A day in the life

```bash
# one-time
sbx setup ssh
sbx secret set gemini -t "$GEMINI_API_KEY"        # + anthropic/openai
cargo build --release                              # host herdr from this fork
export HERDR_TASK_KIT=~/ai-contrib/herdr/kits/herdr-crew/   # or sbx kit push

# per task — directly, or via the leader codex reading ~/ai-contrib/AGENTS.md
git -C ~/src/proj worktree add ../proj-tasks/fix-login
herdr task new fix-login --dir ~/src/proj-tasks/fix-login
herdr task goal fix-login "Users get logged out on refresh; find and fix it, with a regression test."
#   ...watch streams status lines, escalations, and the final RESULT...
herdr task attach fix-login       # optional: watch the four agents live
herdr task rm fix-login           # after reviewing/merging the branch
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

## What was validated, and what wasn't

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

Not yet validated (needs a host with sbx): the end-to-end flow — sandbox
creation from the kit, ssh readiness, inner agent starts, a real
goal-to-RESULT run. The inner herdr is the latest upstream release (0.9.0 in
image 0.1.2), so the driver depends on its published CLI JSON shapes.

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
