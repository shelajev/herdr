import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { readdirSync } from "node:fs";

// Parsed contracts, not source spelling. Behavioral coverage of the task CLI,
// role ids, sandbox argv, completion evidence, and the host-driver update
// policy lives in the Rust tests; this file is about the two things Rust
// cannot see: what the workflows will actually do, and what the kit declares.

const workflowDir = new URL("../.github/workflows/", import.meta.url);
const load = (name: string): any =>
  Bun.YAML.parse(readFileSync(new URL(`${name}.yml`, workflowDir), "utf8"));
const workflowNames = readdirSync(workflowDir)
  .filter((file) => file.endsWith(".yml"))
  .map((file) => file.replace(/\.yml$/, ""));

const CANONICAL = "github.repository == 'herdrdev/herdr'";
const WRITE_PERMISSIONS = [
  "contents",
  "issues",
  "pull-requests",
  "packages",
  "deployments",
  "id-token",
];

/** Jobs that touch a secret or hold a write permission, i.e. can act outward. */
const actsOutward = (job: any, rawWorkflow: string, workflowPermissions: any): boolean => {
  const permissions = job.permissions ?? workflowPermissions;
  const writes =
    permissions &&
    typeof permissions === "object" &&
    WRITE_PERMISSIONS.some((scope) => permissions[scope] === "write");
  const usesSecret = JSON.stringify(job).includes("secrets.");
  return Boolean(writes || usesSecret);
};

describe("fork publishing guards", () => {
  // Every job is checked on its own, so removing one guard fails even when the
  // other jobs in the same workflow keep theirs.
  test.each([
    ["preview", "preflight"],
    ["preview", "publish"],
    ["release", "validate-release-source"],
    ["release", "release"],
    ["release", "update-nix-package"],
    ["release", "close-released-issues"],
    ["release", "update-latest-json"],
    ["website-deploy", "trigger"],
    ["label-next-release-issues", "close"],
    ["pr-gate", "check-contributor"],
  ])("%s job %s is gated to the canonical repository", (workflow, job) => {
    const definition = load(workflow).jobs[job];
    expect(definition).toBeDefined();
    expect(definition.if ?? "").toContain(CANONICAL);
  });

  test("no workflow can act outward from this fork without a guard", () => {
    // Catches a *new* publishing workflow or job arriving in an upstream merge.
    const ungated: string[] = [];
    for (const name of workflowNames) {
      if (name === "fork-sync") continue; // the fork's own automation, gated to the fork
      const workflow = load(name);
      const raw = readFileSync(new URL(`${name}.yml`, workflowDir), "utf8");
      for (const [jobName, job] of Object.entries<any>(workflow.jobs ?? {})) {
        if (!actsOutward(job, raw, workflow.permissions)) continue;
        if (!(job.if ?? "").includes(CANONICAL)) {
          ungated.push(`${name}.${jobName}`);
        }
      }
    }
    expect(ungated).toEqual([]);
  });
});

describe("fork upstream sync workflow", () => {
  const sync = load("fork-sync");
  const job = sync.jobs.sync;
  const steps: any[] = job.steps;
  const step = (fragment: string) =>
    steps.find((candidate) => (candidate.run ?? "").includes(fragment));

  test("runs daily and on demand, serialized, only in the fork", () => {
    expect(sync.on.schedule).toHaveLength(1);
    expect(sync.on.schedule[0].cron).toMatch(/^\S+ \S+ \* \* \*$/);
    expect(sync.on).toHaveProperty("workflow_dispatch");
    expect(sync.concurrency["cancel-in-progress"]).toBe(false);
    expect(job.if).toBe("github.repository == 'shelajev/herdr'");
  });

  test("holds least privilege: read by default, write only where it publishes", () => {
    expect(sync.permissions).toEqual({ contents: "read" });
    expect(job.permissions).toEqual({ contents: "write", "pull-requests": "write" });
    // No new secret: the default token only.
    const used = JSON.stringify(steps).match(/secrets\.[A-Z_]+/g) ?? [];
    expect([...new Set(used)]).toEqual(["secrets.GITHUB_TOKEN"]);
  });

  test("checks out the fork at full depth with pinned actions", () => {
    for (const candidate of steps.filter((entry) => entry.uses)) {
      expect(candidate.uses).toMatch(/@[0-9a-f]{40}$/);
    }
    const checkout = steps.find((entry) => (entry.uses ?? "").includes("actions/checkout"));
    expect(checkout.with["fetch-depth"]).toBe(0);
    expect(checkout.with.ref).toBe("master");
  });

  test("validates the candidate in this workflow, not in downstream CI", () => {
    // A pull request opened with GITHUB_TOKEN does not trigger `pull_request`
    // workflows, so the checks must run here.
    const validate = steps.find((entry) => entry.id === "validate");
    expect(validate.run).toContain("just ci 'all()'");
    expect(validate.run).toContain("just fork-compat-test");
  });

  test("pushes only through the guarded publish path", () => {
    const push = steps.find((entry) => entry.id === "push");
    expect(push.run).toContain("fork_sync.py --repo . publish");
    expect(push.run).toContain("--validated");
    expect(push.if).toContain("steps.validate.outcome == 'success'");

    // No step may push a branch itself: fork_sync.publish() is what re-checks
    // the remote identity, the validated commit, and a clean tree.
    for (const entry of steps) {
      for (const line of (entry.run ?? "").split("\n")) {
        expect(line.trim()).not.toMatch(/^git push\b/);
      }
    }
  });

  test("never rewrites history or merges on its own", () => {
    const body = JSON.stringify(sync);
    for (const forbidden of ["--force", "gh pr merge", "reset --hard", "rebase"]) {
      expect(body).not.toContain(forbidden);
    }
  });

  test("the repository check actually validates", () => {
    const check = step("check-repository");
    expect(check.run).toContain("fork_sync.py --repo . check-repository");
    // `--help` would short-circuit argparse before any validation ran.
    expect(check.run).not.toContain("--help");
  });

  test("scratch files stay out of the validated checkout", () => {
    for (const entry of steps) {
      for (const line of (entry.run ?? "").split("\n")) {
        if (line.includes("candidate.json") || line.includes("pr-body.md")) {
          expect(line).toContain("RUNNER_TEMP");
        }
      }
    }
  });

  test("the pull request targets fork master, never upstream", () => {
    const open = steps.find((entry) => (entry.run ?? "").includes("gh pr create"));
    expect(open.run).toContain("--base master");
    expect(open.run).not.toContain("--repo herdrdev/herdr");
    expect(open.if).toContain("steps.push.outcome == 'success'");
  });
});

describe("platform CI contract", () => {
  const ci = load("ci");
  const matrix: any[] = ci.jobs.check.strategy.matrix.include;

  test("Linux runs the full suite and macOS excludes only live_handoff", () => {
    const linux = matrix.find((entry) => entry.os === "ubuntu-latest");
    const macos = matrix.find((entry) => entry.os === "macos-latest");
    expect(linux.nextest_filter).toBe("all()");
    expect(macos.nextest_filter).toBe("not binary(live_handoff)");
  });

  test("the fork contracts are wired into the suite CI runs", () => {
    const justfile = readFileSync(new URL("../justfile", import.meta.url), "utf8");
    const recipe = (name: string) =>
      justfile.split(`\n${name}`)[1]?.split("\n\n")[0] ?? "";
    expect(recipe("fork-compat-test")).toContain("scripts.test_fork_sync");
    expect(recipe("fork-compat-test")).toContain("scripts/fork-workflows.test.ts");
    // `just ci` is what Linux CI runs, so the contracts gate every change.
    expect(recipe("ci-tests filter=")).toContain("just fork-compat-test");
  });
});

describe("crew kit declaration", () => {
  const kitUrl = new URL("../kits/herdr-crew/spec.yaml", import.meta.url);
  const spec: any = Bun.YAML.parse(readFileSync(kitUrl, "utf8"));

  test("pins all crew starts from one models file", () => {
    expect(spec.args.claude_model.default).toBe("claude-opus-5-5");
    expect(spec.args.codex_model.default).toBe("gpt-6.1-sol");
    expect(spec.args.pi_provider.default).toBe("google");
    expect(spec.args.gemini_model.default).toBe("gemini-3.8-flash");
    expect(spec.environment.variables.HERDR_CREW_CLAUDE_MODEL).toBe("${{ kit.args.claude_model }}");
    expect(spec.environment.variables.HERDR_CREW_CODEX_MODEL).toBe("${{ kit.args.codex_model }}");
    expect(spec.environment.variables.HERDR_CREW_PI_PROVIDER).toBe("${{ kit.args.pi_provider }}");
    const models = spec.setup.files.find((file: any) => file.path === "/home/agent/crew/models");
    expect(models.content).toBe("claude=${{ kit.args.claude_model }},codex=${{ kit.args.codex_model }},pi=${{ kit.args.pi_provider }}/${{ kit.args.gemini_model }}");
    const rest = structuredClone(spec);
    delete rest.args;
    delete rest.environment;
    rest.setup.files = rest.setup.files.filter((file: any) => file.path !== models.path);
    for (const pin of ["claude-opus-5-5", "gpt-6.1-sol", "gemini-3.8-flash"]) {
      expect(JSON.stringify(rest)).not.toContain(pin);
    }
    expect(JSON.stringify(spec.setup.install)).toContain("command -v python3");
  });

  test("keeps the published schema version", () => {
    expect(spec.schemaVersion).toBe("2");
    expect(spec.kind).toBe("sandbox");
    expect(spec.name).toBe("herdr-crew");
  });

  const readme = readFileSync(
    new URL("../kits/herdr-crew/README.md", import.meta.url),
    "utf8",
  );

  test("the source version is distinguished from the published one", () => {
    // Editing the kit in this repository does not republish it. The README has
    // to say which version a new sandbox actually gets, which is the published
    // tag, not whatever this tree declares.
    expect(spec.version).toMatch(/^\d+\.\d+\.\d+$/);
    expect(readme).toContain(`kit version **${spec.version}**`);
    expect(readme).toContain("not published yet");
    expect(readme).toContain("`:0.4.5`");
    expect(spec.version).not.toBe("0.4.5");
  });

  test("the inner Herdr and agent CLIs have pinned template build defaults", () => {
    // The template bakes an ordinary upstream release; `latest` would make two
    // builds of the same source produce different images.
    const dockerfile = readFileSync(
      new URL("../kits/herdr-crew/template/Dockerfile", import.meta.url),
      "utf8",
    );
    const pin = dockerfile.match(/^ARG HERDR_VERSION=(.+)$/m);
    expect(pin).not.toBeNull();
    expect(pin![1].trim()).toMatch(/^\d+\.\d+\.\d+$/);
    expect(dockerfile).toContain("ARG CLAUDE_CODE_VERSION=2.1.289\n");
    expect(dockerfile).toContain("ARG CODEX_VERSION=0.153.4\n");
    expect(dockerfile).toContain("ARG PI_VERSION=0.85.1\n");
  });

  test("ships a brief for every native crew role", () => {
    const roles = spec.args.roles.default
      .split(",")
      .map((pair: string) => pair.split("=")[0]);
    expect(roles.sort()).toEqual(["implementer", "orchestrator", "planner", "qc"]);
    for (const role of roles) {
      const brief = new URL(
        `../kits/herdr-crew/files/home/crew/roles/${role}.md`,
        import.meta.url,
      );
      expect(readFileSync(brief, "utf8").length).toBeGreaterThan(0);
    }
  });

  test("the default assignment keeps qc independent of the implementer", () => {
    const roles = Object.fromEntries(
      spec.args.roles.default.split(",").map((pair: string) => pair.split("=")),
    );
    expect(roles.qc).not.toBe(roles.implementer);
  });

  test("network policy is an explicit allow list, never open", () => {
    const allow: string[] = spec.permissions.network.allow;
    expect(allow).not.toContain("*");
    expect(allow.every((host) => !host.includes("*"))).toBe(true);
    // Every credential injection domain must also be reachable.
    for (const credential of spec.credentials ?? []) {
      for (const injection of credential.apiKey?.inject ?? []) {
        expect(allow).toContain(injection.domain);
      }
    }
    // Remote agent-detection manifests.
    expect(allow).toContain("herdr.dev");
  });

  test("boots from a published image and never builds one at creation", () => {
    expect(spec.sandbox.image).toBe("${{ kit.args.image }}");
    expect(spec.args.image.default).toMatch(/^docker\.io\//);
    expect(spec.args.image.default).toBe("docker.io/olegselajev241/herdr-crew:0.5.1");
    expect(spec.sandbox.build).toBeUndefined();
  });

  test("supervises the server with a real request in detached sandboxes", () => {
    const startup: any[] = spec.setup.startup;
    const supervisor = startup.find((entry) =>
      JSON.stringify(entry.command).includes("herdr agent list"),
    );
    expect(supervisor).toBeDefined();
    expect(supervisor.background).toBe(true);
    expect(supervisor.user).toBe("agent");
    // A pgrep guard can match the probe process itself.
    expect(JSON.stringify(startup)).not.toContain("pgrep");
  });

  test("disables the Codex startup updater an unattended crew cannot answer", () => {
    const install = JSON.stringify(spec.setup.install);
    expect(install).toContain("check_for_update_on_startup = false");
  });

  test("trusts only the task workspace in Codex, never a blanket path", () => {
    const step = spec.setup.install.find((entry: any) =>
      entry.description.startsWith("Seed Codex trust"),
    );
    expect(step).toBeDefined();
    expect(step.user).toBe("agent");
    expect(step.command).toContain('ws="${WORKSPACE_DIR:-}"');
    expect(step.command).toContain('trust_level = "trusted"');
    expect(step.command).not.toContain('projects."/"');
    expect(step.command).not.toContain("WORKSPACE_DIR:-/");
  });

  test("hands the host the paths it reads back", () => {
    const paths = spec.setup.files.map((file: any) => file.path);
    expect(paths).toContain("/home/agent/crew/assignment");
    expect(paths).toContain("/home/agent/crew/workdir");
  });
});
