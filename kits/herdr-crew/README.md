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
herdr task goal <slug>   ── ssh ────────▶ orchestrator  (holds /goal)
  then only watches status                  ├─▶ planner      writes plan.md
herdr task watch <slug>  ◀─ status.md ──    ├─▶ implementer  commits work
herdr task policy <slug> ◀─ escalations     └─▶ qc           VERDICT lines
  grant/deny resources                    orchestrator appends RESULT: DONE|
herdr task attach <slug> ── live view      FAILED|BLOCKED to status.md
```

## Components

- **`herdr task` CLI** (`src/cli/task.rs`, pure logic in `src/tasks.rs`):
  `new`, `goal`, `watch`, `ls`, `status`, `attach`, `policy`, `rm`. The host
  starts only the orchestrator and delivers the goal; the orchestrator
  assembles and drives the rest of the crew through the *inner* herdr CLI.
- **This kit** (`spec.yaml`): sandbox kit pointing at the template image;
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
  to `status.md`; `herdr task watch` tails status/escalations and exits 0/1/3
  respectively. It must not write `DONE` unless qc's latest verdict is PASS.
- **Escalation**: any crew member hitting a blocked domain records it in
  `escalations.md`; the watch surfaces it; the host (you or the leader codex)
  decides: `herdr task policy <slug> --allow <domain>`, then resume with
  `herdr task goal <slug> "resources updated, continue"`.

## One-time host setup

```bash
sbx setup ssh                             # every sandbox becomes <name>.sbx
sbx secret set gemini -t "$GEMINI_API_KEY"   # + anthropic/openai API keys
cargo build --release                        # host herdr with the task commands
```

The kit is published as `docker.io/olegselajev241/herdr-crew-kit:latest`
(also `:0.2.0`) — the CLI's default — so no local kit reference is needed.
When iterating on the kit itself, point `HERDR_TASK_KIT` (or `--kit`) at
`./kits/herdr-crew/` and republish with
`sbx kit push ./kits/herdr-crew docker.io/olegselajev241/herdr-crew-kit:latest`.

## Per task

```bash
cd ~/src/some-project
herdr task new fix-login                  # rotation picks the four roles
herdr task new yt-digest \
  --mixin "git+https://github.com/shelajev/yt-transcript-sbx-kit.git"
                                          # stack extra tools onto the crew sandbox;
                                          # the mixin's agent memory teaches the crew its tools
herdr task goal fix-login "Users get logged out on refresh; find and fix it, with a regression test."
# delivers the goal and watches; Ctrl-C detaches, the crew keeps working
herdr task watch fix-login                # re-attach to progress any time
herdr task attach fix-login               # live TUI view of the whole crew
herdr task rm fix-login
```

The workspace directory mounts into the sandbox at the same absolute path;
commits land in it directly — use a dedicated clone or worktree per task.

Mixins install as root inside the sandbox, so treat a mixin reference as code
you're choosing to run. sbx's kit-source allowlist defaults to `docker.io/`;
git-hosted kits need a one-time host setting, e.g.:

```bash
sbx settings set kit.allowedSources '["docker.io/","github.com/shelajev/"]'
```

## Model notes

- The Google-models member is pi (pi.dev): it reads `GEMINI_API_KEY`, defaults
  to the google provider, and the driver pins `--model gemini-3.8-flash` (kit
  arg `gemini_model`; pi 0.85+ ships the model in its catalog).
- Rotation is deterministic from the slug (18 assignments: 6 crew
  permutations x 3 orchestrator choices), reproducible per slug, varied
  across slugs to balance subscription usage.

## Known limitations (v0)

- Task state lives in sbx (sandbox name) and the sandbox's crew files; the
  host server keeps no task registry yet (planned: `task.*` JSON API).
- The inner herdr is the latest upstream release binary; the orchestrator and
  the host driver depend on its CLI shapes.
- Template image is linux/arm64 only; API-key auth only until the
  subscription-OAuth kit blocks are verified.
- `RESULT:`/`VERDICT:` lines are convention enforced by prompts, not code;
  a misbehaving orchestrator can misreport — `task attach` and the qc log are
  the audit trail.
