//! `herdr task` — host-side lifecycle for crew task sandboxes.
//!
//! The host herdr provisions one Docker SBX sandbox per task from the
//! herdr-crew kit and delivers the goal to the *sandboxed orchestrator* agent
//! over SSH (`sbx setup ssh` exposes every sandbox as `<name>.sbx`). The
//! goal-seeking intelligence stays inside the sandbox by design: the host
//! never drives the crew, it only kicks tasks off, watches the crew's status
//! file, and arbitrates resource escalations (`task policy`). Quality control
//! is never performed by the model that implemented the work; role rotation
//! and that invariant live in `crate::tasks`.

use std::process::{Command, Stdio};
use std::time::Duration;

use crate::tasks::{
    self, parse_result, parse_roles, resolve_mcp_names, rotation_for, sandbox_name,
    sbx_create_argv, shell_quote, ssh_host, validate_slug, RoleAssignment, TaskResult,
};

const CREW_DIR: &str = "/home/agent/crew";
const READY_POLL_ATTEMPTS: u32 = 36;
const READY_POLL_DELAY: Duration = Duration::from_secs(5);
const AGENT_START_TIMEOUT_MS: u64 = 240_000;
const WATCH_POLL_DELAY: Duration = Duration::from_secs(20);
/// Where the kit mounts the reviewed project inside the sandbox.
const DEFAULT_WORKSPACE_DIR: &str = "/home/agent/workspace";
/// `herdr task watch` exit code for a run that reported DONE without evidence
/// that an independent reviewer accepted its current commit. Distinct from
/// BLOCKED (3) so automation can tell "needs resources" from "not reviewed".
const EXIT_UNACCEPTED: i32 = 4;
/// Native delivery succeeded but the orchestrator did not acknowledge this run.
const EXIT_UNACKNOWLEDGED: i32 = 6;
const GOAL_ACK_TIMEOUT: Duration = Duration::from_secs(120);

pub(super) fn run_task_command(args: &[String]) -> std::io::Result<i32> {
    let Some(subcommand) = args.first().map(|arg| arg.as_str()) else {
        print_task_help();
        return Ok(2);
    };

    let result = match subcommand {
        "new" => task_new(&args[1..]),
        "ls" | "list" => task_ls(),
        "status" => task_status(&args[1..]),
        "goal" => task_goal(&args[1..]),
        "watch" => task_watch(&args[1..]),
        "attach" => task_attach(&args[1..]),
        "policy" => task_policy(&args[1..]),
        "rm" | "remove" => task_rm(&args[1..]),
        "help" | "--help" | "-h" => {
            print_task_help();
            Ok(0)
        }
        _ => {
            print_task_help();
            Ok(2)
        }
    };
    match result {
        Ok(exit_code) => Ok(exit_code),
        Err(err) => {
            eprintln!("{err}");
            Ok(1)
        }
    }
}

fn print_task_help() {
    eprintln!("usage:");
    eprintln!(
        "  herdr task new <slug> [--dir PATH] [--kit REF] [--mixin REF]... [--mcp NAME]... [--claude-model ID] [--codex-model ID] [--pi-provider ID] [--pi-model ID] [--roles orchestrator=K,planner=K,implementer=K,qc=K]"
    );
    eprintln!("  herdr task goal <slug> <text> [--no-watch] [--report-only]");
    eprintln!("  herdr task watch <slug>");
    eprintln!("  herdr task ls");
    eprintln!("  herdr task status <slug>");
    eprintln!("  herdr task attach <slug>");
    eprintln!("  herdr task policy <slug> [--allow DOMAIN]");
    eprintln!("  herdr task rm <slug>");
    eprintln!();
    eprintln!("Tasks run in Docker SBX sandboxes provisioned from the herdr-crew kit.");
    eprintln!("Model override precedence: flags, then HERDR_TASK_CLAUDE_MODEL,");
    eprintln!(
        "HERDR_TASK_CODEX_MODEL, HERDR_TASK_PI_PROVIDER, HERDR_TASK_PI_MODEL, then kit defaults."
    );
    eprintln!(
        "--mcp NAME attaches a static MCP server already registered on this host (sbx mcp add);"
    );
    eprintln!(
        "{} (comma-separated) is the default when no --mcp is given.",
        tasks::MCP_ENV_VAR
    );
    eprintln!("Requires the sbx CLI and one-time `sbx setup ssh`. The kit reference");
    eprintln!(
        "defaults to {} (override with --kit or {}).",
        tasks::DEFAULT_KIT,
        tasks::KIT_ENV_VAR
    );
}

fn kit_reference(explicit: Option<String>) -> String {
    explicit
        .or_else(|| std::env::var(tasks::KIT_ENV_VAR).ok())
        .unwrap_or_else(|| tasks::DEFAULT_KIT.to_string())
}

// ---------------------------------------------------------------------------
// Local process helpers
// ---------------------------------------------------------------------------

fn run_inherited(argv: &[String]) -> std::io::Result<i32> {
    let Some((program, rest)) = argv.split_first() else {
        return Err(std::io::Error::other("empty command"));
    };
    let status = Command::new(program)
        .args(rest)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|err| std::io::Error::other(format!("failed to run {program}: {err}")))?;
    Ok(status.code().unwrap_or(1))
}

struct RemoteOutput {
    exit_code: i32,
    stdout: String,
    stderr: String,
}

/// Run one command line on the task sandbox over SSH and capture its output.
fn ssh_capture(host: &str, remote_command: &str) -> std::io::Result<RemoteOutput> {
    let output = Command::new("ssh")
        .args(["-o", "BatchMode=yes", host, remote_command])
        .stdin(Stdio::null())
        .output()
        .map_err(|err| std::io::Error::other(format!("failed to run ssh: {err}")))?;
    Ok(RemoteOutput {
        exit_code: output.status.code().unwrap_or(1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// ACK polling must bound the SSH read too, not just the delay between polls.
fn read_goal_ack(host: &str, timeout: Duration) -> std::io::Result<String> {
    let child = Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            host,
            "cat /home/agent/crew/status.md",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let output = crate::remote::wait_with_output_timeout(child, timeout)?;
    if !output.status.success() {
        return Err(std::io::Error::other(
            "could not read the goal acknowledgment",
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn ssh_herdr_json(
    host: &str,
    herdr_args: &[&str],
) -> std::io::Result<Result<serde_json::Value, String>> {
    let mut argv = vec!["herdr"];
    argv.extend_from_slice(herdr_args);
    let remote = tasks::remote_command(&argv);
    let output = ssh_capture(host, &remote)?;
    if output.exit_code != 0 {
        let detail = if output.stderr.trim().is_empty() {
            output.stdout.trim().to_string()
        } else {
            output.stderr.trim().to_string()
        };
        return Ok(Err(format!(
            "remote `{remote}` failed (exit {}): {detail}",
            output.exit_code
        )));
    }
    match serde_json::from_str(&output.stdout) {
        Ok(value) => Ok(Ok(value)),
        Err(err) => Ok(Err(format!(
            "remote `{remote}` returned unparseable output ({err}): {}",
            output.stdout.trim()
        ))),
    }
}

fn read_crew_file(host: &str, name: &str) -> std::io::Result<Option<String>> {
    let path = format!("{CREW_DIR}/{name}");
    let remote = format!("cat {} 2>/dev/null || true", shell_quote(&path));
    let output = ssh_capture(host, &remote)?;
    let content = output.stdout;
    if content.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(content))
    }
}

/// Marker the remote prints when an evidence file does not exist, so an absent
/// file is distinguishable from an unreadable sandbox.
const NO_SUCH_CREW_FILE: &str = "__herdr_no_such_crew_file__";

/// Read a crew file for acceptance purposes.
///
/// `read_crew_file` swallows transport failures, which is right for tailing a
/// status file and wrong for deciding whether work is done: a broken SSH
/// connection must not read as "the crew produced no QC report". The outer
/// `Err` is a transport failure, `Ok(None)` is a genuinely absent file.
fn read_evidence_file(host: &str, name: &str) -> std::io::Result<Result<Option<String>, String>> {
    let path = shell_quote(&format!("{CREW_DIR}/{name}"));
    let remote =
        format!("if [ -r {path} ]; then cat {path}; else printf %s {NO_SUCH_CREW_FILE}; fi");
    let output = ssh_capture(host, &remote)?;
    if output.exit_code != 0 {
        return Ok(Err(format!(
            "could not read {CREW_DIR}/{name} from {host} (exit {}): {}",
            output.exit_code,
            output.stderr.trim()
        )));
    }
    if output.stdout.trim() == NO_SUCH_CREW_FILE {
        return Ok(Ok(None));
    }
    Ok(Ok(Some(output.stdout)))
}

/// Write one crew file from the host, replacing any previous content.
fn write_crew_file(host: &str, name: &str, content: &str) -> std::io::Result<Result<(), String>> {
    let path = shell_quote(&format!("{CREW_DIR}/{name}"));
    // The heredoc body is data, never shell: 'EOF' is quoted so nothing in the
    // content is expanded, and the content itself is never interpolated into
    // the command line.
    let remote = format!("cat > {path} <<'HERDR_EOF'\n{content}\nHERDR_EOF\n");
    let output = ssh_capture(host, &remote)?;
    if output.exit_code != 0 {
        return Ok(Err(format!(
            "could not write {CREW_DIR}/{name} on {host} (exit {}): {}",
            output.exit_code,
            output.stderr.trim()
        )));
    }
    Ok(Ok(()))
}

/// Remove a crew file, ignoring an already-absent one.
fn remove_crew_file(host: &str, name: &str) -> std::io::Result<()> {
    let path = shell_quote(&format!("{CREW_DIR}/{name}"));
    let _ = ssh_capture(host, &format!("rm -f {path}"))?;
    Ok(())
}

/// Run a git command in the sandboxed workspace and capture stdout.
fn remote_git(host: &str, workdir: &str, args: &[&str]) -> std::io::Result<Result<String, String>> {
    let mut argv = vec!["git", "-C", workdir];
    argv.extend_from_slice(args);
    let remote = tasks::remote_command(&argv);
    let output = ssh_capture(host, &remote)?;
    if output.exit_code != 0 {
        return Ok(Err(format!(
            "remote `{remote}` failed (exit {}): {}",
            output.exit_code,
            output.stderr.trim()
        )));
    }
    Ok(Ok(output.stdout))
}

/// Decide whether a `RESULT: DONE` may be accepted for this run.
///
/// The run id and the workspace HEAD are read both before and after the QC
/// report, so a crew that commits or edits while the host is mid-poll fails
/// instead of slipping a stale report through. Any unreadable input is a
/// refusal, never an acceptance.
fn accept_completed_run(
    host: &str,
    workdir: &str,
    roles: &RoleAssignment,
) -> std::io::Result<Result<tasks::QcEvidence, String>> {
    macro_rules! bail {
        ($value:expr) => {
            match $value {
                Ok(value) => value,
                Err(err) => return Ok(Err(err)),
            }
        };
    }

    let run_before = bail!(read_evidence_file(host, tasks::RUN_METADATA_FILE)?);
    let head_before = bail!(remote_git(host, workdir, &["rev-parse", "HEAD"])?);
    let porcelain_before = bail!(remote_git(host, workdir, &["status", "--porcelain"])?);
    let qc = bail!(read_evidence_file(host, tasks::QC_EVIDENCE_FILE)?);
    let Some(run_raw) = run_before.as_deref() else {
        return Ok(Err(
            tasks::EvidenceError::Missing(tasks::RUN_METADATA_FILE).to_string()
        ));
    };
    let run = match tasks::RunMetadata::parse(run_raw) {
        Ok(run) => run,
        Err(err) => return Ok(Err(err.to_string())),
    };
    if !tasks::is_full_commit_id(&run.base_commit) || !tasks::is_full_commit_id(head_before.trim())
    {
        return Ok(Err("base_commit and HEAD must be full commit ids".into()));
    }
    // Keep ancestry inside the same read window as the report and HEAD.
    // Non-ancestry and an unreadable Git graph both refuse acceptance.
    if run.scope == tasks::RunScope::Change {
        bail!(remote_git(
            host,
            workdir,
            &[
                "merge-base",
                "--is-ancestor",
                &run.base_commit,
                head_before.trim()
            ]
        )?);
    }
    let porcelain = bail!(remote_git(host, workdir, &["status", "--porcelain"])?);
    let head_after = bail!(remote_git(host, workdir, &["rev-parse", "HEAD"])?);
    let run_after = bail!(read_evidence_file(host, tasks::RUN_METADATA_FILE)?);

    if porcelain_before != porcelain {
        return Ok(Err(
            "the workspace changed while QC evidence was being read".into(),
        ));
    }
    if head_before.trim() != head_after.trim() {
        return Ok(Err(format!(
            "the workspace moved from {} to {} while its QC evidence was being read; \
             a new QC round is required",
            head_before.trim(),
            head_after.trim()
        )));
    }
    if run_before != run_after {
        return Ok(Err(
            "a new run started while its QC evidence was being read".to_string(),
        ));
    }

    match tasks::accept_run(
        roles,
        run_before.as_deref(),
        qc.as_deref(),
        head_before.trim(),
        &porcelain,
        true,
    ) {
        Ok(evidence) => Ok(Ok(evidence)),
        Err(err) => Ok(Err(err.to_string())),
    }
}

// ---------------------------------------------------------------------------
// task new
// ---------------------------------------------------------------------------

fn task_new(args: &[String]) -> std::io::Result<i32> {
    let Some(slug) = args.first().cloned() else {
        eprintln!(
            "usage: herdr task new <slug> [--dir PATH] [--kit REF] [--mixin REF]... [--mcp NAME]... [--claude-model ID] [--codex-model ID] [--pi-provider ID] [--pi-model ID] [--roles ...]"
        );
        return Ok(2);
    };
    if let Err(err) = validate_slug(&slug) {
        eprintln!("{err}");
        return Ok(2);
    }

    let mut dir = None;
    let mut kit = None;
    let mut roles_arg = None;
    let mut models = tasks::ModelOverrides::default();
    let mut mixins: Vec<String> = Vec::new();
    let mut mcp_flags: Vec<String> = Vec::new();
    let mut index = 1;
    while index < args.len() {
        let (option, value) = (args[index].as_str(), args.get(index + 1));
        match option {
            "--dir" | "--kit" | "--roles" | "--mixin" | "--mcp" | "--claude-model"
            | "--codex-model" | "--pi-provider" | "--pi-model" => {
                let Some(value) = value else {
                    eprintln!("missing value for {option}");
                    return Ok(2);
                };
                match option {
                    "--dir" => dir = Some(value.clone()),
                    "--kit" => kit = Some(value.clone()),
                    "--mixin" => mixins.push(value.clone()),
                    "--mcp" => mcp_flags.push(value.clone()),
                    "--claude-model" => models.claude = Some(value.clone()),
                    "--codex-model" => models.codex = Some(value.clone()),
                    "--pi-provider" => models.pi_provider = Some(value.clone()),
                    "--pi-model" => models.pi_model = Some(value.clone()),
                    _ => roles_arg = Some(value.clone()),
                }
                index += 2;
            }
            other => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
        }
    }

    models = models.with_fallback(tasks::ModelOverrides {
        claude: std::env::var("HERDR_TASK_CLAUDE_MODEL").ok(),
        codex: std::env::var("HERDR_TASK_CODEX_MODEL").ok(),
        pi_provider: std::env::var("HERDR_TASK_PI_PROVIDER").ok(),
        pi_model: std::env::var("HERDR_TASK_PI_MODEL").ok(),
    });
    let model_args = match models.kit_args() {
        Ok(args) => args,
        Err(err) => {
            eprintln!("invalid model override: {err}");
            return Ok(2);
        }
    };
    // Static MCP servers are host-registered resources referenced by name; a
    // bad name stops here, before any sandbox is provisioned.
    let mcps = match resolve_mcp_names(
        &mcp_flags,
        std::env::var(tasks::MCP_ENV_VAR).ok().as_deref(),
    ) {
        Ok(mcps) => mcps,
        Err(err) => {
            eprintln!("invalid MCP server: {err}");
            return Ok(2);
        }
    };
    let roles = match roles_arg {
        Some(value) => match parse_roles(&value) {
            Ok(roles) => roles,
            Err(err) => {
                eprintln!("{err}");
                return Ok(2);
            }
        },
        None => rotation_for(&slug),
    };
    let workspace = std::fs::canonicalize(dir.unwrap_or_else(|| ".".to_string()))?;
    let Some(workspace) = workspace.to_str().map(str::to_string) else {
        eprintln!("workspace path is not valid UTF-8");
        return Ok(2);
    };
    // A linked git worktree keeps its object store in the parent repository,
    // which is not mounted into the sandbox — git inside the crew would be
    // broken. Fail early with the fix instead of provisioning a doomed task.
    let git_pointer = std::path::Path::new(&workspace).join(".git");
    if git_pointer.is_file() {
        let gitdir = std::fs::read_to_string(&git_pointer).unwrap_or_default();
        if !gitdir
            .trim()
            .strip_prefix("gitdir:")
            .map(str::trim)
            .is_some_and(|path| path.starts_with(&workspace))
        {
            eprintln!(
                "{workspace} is a linked git worktree; its repository lives outside the \
                 workspace and will not be mounted into the sandbox, so git would be broken \
                 for the crew."
            );
            eprintln!("use a standalone clone instead, e.g.:");
            eprintln!("  git clone <repo> {workspace}-clone && herdr task new {slug} --dir {workspace}-clone");
            return Ok(2);
        }
    }
    let kit = kit_reference(kit);

    println!("task {slug}: sandbox {}", sandbox_name(&slug));
    println!("  workspace: {workspace}");
    println!("  kit:       {kit}");
    println!(
        "  roles:     orchestrator={} planner={} implementer={} qc={}",
        roles.orchestrator, roles.planner, roles.implementer, roles.qc
    );
    for mixin in &mixins {
        println!("  mixin:     {mixin}");
    }
    for mcp in &mcps {
        println!("  mcp:       {mcp}");
    }

    let argv = sbx_create_argv(&kit, &workspace, &slug, &roles, &mixins, &mcps, &model_args);
    let exit_code = run_inherited(&argv)?;
    if exit_code != 0 {
        eprintln!("sbx create failed (exit {exit_code})");
        if !mcps.is_empty() {
            eprintln!(
                "each --mcp NAME must already be registered on this host (`sbx mcp add`); \
                 check the name(s) above"
            );
        }
        return Ok(1);
    }

    println!("waiting for the herdr server inside the sandbox...");
    let host = ssh_host(&slug);
    for attempt in 1..=READY_POLL_ATTEMPTS {
        // Require a real server request: status text can describe stale state.
        let probe = server_running_probe(&host)?;
        if probe.exit_code == 0 {
            let pins = match crew_models(&host) {
                Ok(pins) => pins,
                Err(err) => {
                    eprintln!("{err}");
                    return Ok(1);
                }
            };
            println!("  models:    {}", pins.file_value());
            println!("task {slug} is ready.");
            println!("  herdr task goal {slug} \"<what to build>\"");
            println!("  herdr task attach {slug}");
            return Ok(0);
        }
        if attempt == 1 && probe.stderr.contains("Could not resolve hostname") {
            eprintln!(
                "hint: run `sbx setup ssh` once on this host to enable <name>.sbx SSH access."
            );
        }
        // The kit's startup hook normally starts the server; nudge it in case
        // this sandbox predates that hook or the hook lost a race.
        if attempt == 3 {
            let _ = ssh_capture(&host, START_SERVER_REMOTE)?;
        }
        std::thread::sleep(READY_POLL_DELAY);
    }
    eprintln!(
        "sandbox created, but the herdr server did not become reachable over ssh {host}; \
         inspect it with `ssh {host}` or `sbx attach {}`",
        sandbox_name(&slug)
    );
    Ok(1)
}

/// Start only after a failed server request. A pgrep guard can match itself.
/// Give the server its own session so SSH cleanup cannot terminate its process group.
const START_SERVER_REMOTE: &str =
    "nohup setsid -f herdr server >>/tmp/herdr-server.log 2>&1 </dev/null &";

fn server_running_probe(host: &str) -> std::io::Result<RemoteOutput> {
    ssh_capture(host, "herdr agent list >/dev/null 2>&1")
}

/// Make sure the inner herdr server is actually up before talking to it.
fn ensure_server(host: &str) -> std::io::Result<bool> {
    if server_running_probe(host)?.exit_code == 0 {
        return Ok(true);
    }
    let _ = ssh_capture(host, START_SERVER_REMOTE)?;
    for _ in 0..6 {
        std::thread::sleep(Duration::from_secs(2));
        if server_running_probe(host)?.exit_code == 0 {
            return Ok(true);
        }
    }
    Ok(false)
}

// ---------------------------------------------------------------------------
// task goal — deliver the goal to the sandboxed orchestrator
// ---------------------------------------------------------------------------

fn task_goal(args: &[String]) -> std::io::Result<i32> {
    // --no-watch may appear anywhere, including before the positionals.
    let mut watch = true;
    let mut scope = tasks::RunScope::Change;
    let mut positionals = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--no-watch" => watch = false,
            "--report-only" => scope = tasks::RunScope::ReportOnly,
            other if other.starts_with("--") => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
            _ => positionals.push(arg.clone()),
        }
    }
    let (Some(slug), Some(goal)) = (positionals.first().cloned(), positionals.get(1).cloned())
    else {
        eprintln!("usage: herdr task goal <slug> <text> [--no-watch] [--report-only]");
        return Ok(2);
    };
    if let Err(err) = validate_slug(&slug) {
        eprintln!("{err}");
        return Ok(2);
    }

    let host = ssh_host(&slug);
    if !ensure_server(&host)? {
        eprintln!(
            "the herdr server inside {host} is not running and could not be started; \
             inspect with `ssh {host} cat /tmp/herdr-server.log`"
        );
        return Ok(1);
    }
    let Some(roles) = crew_assignment(&host)? else {
        eprintln!(
            "cannot read the crew assignment from {host}; does the task exist? \
             (herdr task new {slug})"
        );
        return Ok(1);
    };
    let workdir = read_crew_file(&host, "workdir")?
        .map(|content| content.trim().to_string())
        .unwrap_or_else(|| DEFAULT_WORKSPACE_DIR.to_string());

    println!(
        "task {slug}: orchestrator={} planner={} implementer={} qc={} (workspace {workdir})",
        roles.orchestrator, roles.planner, roles.implementer, roles.qc
    );

    // Only the orchestrator is started from the host; it assembles the rest of
    // the crew itself. The goal-seeking side stays inside the sandbox.
    if let Err(err) = ensure_agent(&host, "orchestrator", &roles, &workdir) {
        eprintln!("failed to start the orchestrator: {err}");
        eprintln!("inspect with: herdr task attach {slug}");
        return Ok(1);
    }

    // Check before minting metadata: an old active turn must not be mistaken
    // for acknowledgment of a new parallel goal. This does not preserve runs.
    let orchestrator = match ssh_herdr_json(&host, &["agent", "get", "orchestrator"])? {
        Ok(value) => value,
        Err(err) => {
            eprintln!("cannot inspect the orchestrator: {err}");
            return Ok(1);
        }
    };
    let Some(orchestrator_status) = orchestrator
        .pointer("/result/agent/agent_status")
        .and_then(|v| v.as_str())
    else {
        eprintln!("cannot read the orchestrator's native status");
        return Ok(1);
    };
    let status = match read_evidence_file(&host, "status.md")? {
        Ok(value) => value.unwrap_or_default(),
        Err(err) => {
            eprintln!("{err}");
            return Ok(1);
        }
    };
    if tasks::goal_delivery_decision(orchestrator_status, tasks::current_run_slice(&status))
        == tasks::GoalDeliveryDecision::Busy
    {
        let current_run = match read_evidence_file(&host, tasks::RUN_METADATA_FILE)? {
            Ok(Some(raw)) => tasks::RunMetadata::parse(&raw).ok().map(|run| run.run_id),
            _ => None,
        }
        .unwrap_or_else(|| "unknown".into());
        eprintln!("run {current_run} is still in progress; use `herdr task watch {slug}`");
        return Ok(1);
    }

    // Mint this run's identity before every goal delivery. The QC report must
    // name this run id and this run's goal, so a report left behind by an
    // earlier run can never be mistaken for acceptance of this one. Discarding
    // any previous report is part of the same step: evidence describes one
    // reviewed commit of one run, and it is invalid the moment a new run opens.
    let base_commit = match remote_git(&host, &workdir, &["rev-parse", "HEAD"])? {
        Ok(head) if tasks::is_full_commit_id(head.trim()) => head.trim().to_string(),
        _ => {
            eprintln!("cannot start a task goal: the workspace must have a readable commit (HEAD)");
            return Ok(1);
        }
    };
    match remote_git(&host, &workdir, &["status", "--porcelain"])? {
        Ok(porcelain) if !porcelain.trim().is_empty() => eprintln!("warning: starting from a dirty workspace (including untracked files); QC requires a clean tree"),
        Ok(_) => {},
        Err(err) => { eprintln!("cannot read the starting worktree: {err}"); return Ok(1); }
    }
    let run = tasks::new_run_metadata(&goal, &workdir, &base_commit, scope);
    remove_crew_file(&host, tasks::QC_EVIDENCE_FILE)?;
    if let Err(err) = write_crew_file(&host, tasks::RUN_METADATA_FILE, &run.to_json())? {
        eprintln!("failed to record this run's identity: {err}");
        return Ok(1);
    }

    // Scope this run: RESULT lines before the marker belong to previous goals
    // and must not terminate this run's watch.
    let marker = format!(
        "printf '\\n%s\\n' {} >> {}",
        shell_quote(tasks::GOAL_MARKER),
        shell_quote(&format!("{CREW_DIR}/status.md"))
    );
    let _ = ssh_capture(&host, &marker)?;

    let goal_prompt = format!(
        "HANDOFF run={} phase=goal role=orchestrator\nYou are the crew orchestrator for this task. Read {CREW_DIR}/roles/orchestrator.md \
         and follow it exactly: assemble the crew from {CREW_DIR}/assignment, coordinate \
         plan -> implement -> qc rounds until qc passes, record progress in \
         {CREW_DIR}/status.md, and finish by appending a final line \
         'RESULT: DONE', 'RESULT: FAILED', or 'RESULT: BLOCKED' to {CREW_DIR}/status.md. \
         The workspace is {workdir}. /goal: {goal}",
        run.run_id
    );
    // Starting a TUI can briefly look idle before its final initialization
    // redraw. Confirm the prompt actually triggered work rather than treating
    // a successful input write as successful goal delivery.
    if let Err(err) = ssh_herdr_json(
        &host,
        &[
            "agent",
            "prompt",
            "orchestrator",
            &goal_prompt,
            "--wait",
            "--until",
            "working",
            "--until",
            "blocked",
            "--timeout",
            "60000",
        ],
    )? {
        eprintln!("failed to deliver the goal: {err}");
        return Ok(1);
    }
    let ack_deadline = std::time::Instant::now() + GOAL_ACK_TIMEOUT;
    loop {
        let remaining = ack_deadline.saturating_duration_since(std::time::Instant::now());
        match read_goal_ack(&host, remaining) {
            Ok(status) if tasks::run_acknowledged(&status, &run.run_id) => break,
            Err(_) => {
                eprintln!("native goal delivery succeeded, but status.md could not be read to verify ACK run={}; inspect with `herdr task attach {slug}`", run.run_id);
                return Ok(EXIT_UNACKNOWLEDGED);
            }
            _ => {}
        }
        if std::time::Instant::now() >= ack_deadline {
            eprintln!("native goal delivery succeeded, but ACK run={} was not observed in status.md within 120 seconds; inspect with `herdr task attach {slug}`", run.run_id);
            return Ok(EXIT_UNACKNOWLEDGED);
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    println!("goal delivered and acknowledged by the sandboxed orchestrator.");
    if !watch {
        println!("follow progress with: herdr task watch {slug}");
        return Ok(0);
    }
    watch_task(&slug, &host, &workdir, &roles)
}

// ---------------------------------------------------------------------------
// task watch — tail crew status and escalations until a RESULT line
// ---------------------------------------------------------------------------

fn task_watch(args: &[String]) -> std::io::Result<i32> {
    let Some(slug) = args.first() else {
        eprintln!("usage: herdr task watch <slug>");
        return Ok(2);
    };
    if let Err(err) = validate_slug(slug) {
        eprintln!("{err}");
        return Ok(2);
    }
    let host = ssh_host(slug);
    let workdir = read_crew_file(&host, "workdir")?
        .map(|content| content.trim().to_string())
        .unwrap_or_else(|| DEFAULT_WORKSPACE_DIR.to_string());
    let Some(roles) = crew_assignment(&host)? else {
        eprintln!(
            "cannot read the crew assignment from {host}; without it there is no configured \
             reviewer to check a completion report against"
        );
        return Ok(1);
    };
    watch_task(slug, &host, &workdir, &roles)
}

/// Print what changed in a crew file since the previous poll. Content-based
/// (not line counts) so in-place rewrites and truncations are detected.
fn print_file_delta(prefix: &str, previous: &str, current: &str) -> bool {
    if current == previous {
        return false;
    }
    if let Some(added) = current.strip_prefix(previous) {
        for line in added.lines() {
            println!("{prefix} | {line}");
        }
    } else {
        println!("{prefix} | (file rewritten; current content follows)");
        for line in current.lines() {
            println!("{prefix} | {line}");
        }
    }
    true
}

fn watch_task(
    slug: &str,
    host: &str,
    workdir: &str,
    roles: &RoleAssignment,
) -> std::io::Result<i32> {
    println!("watching task {slug} (Ctrl-C to stop; the crew keeps working)...");
    let mut last_status = String::new();
    let mut last_escalations = String::new();
    let mut agent_states: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    loop {
        let status = read_crew_file(host, "status.md")?.unwrap_or_default();
        print_file_delta("status", &last_status, &status);
        last_status = status.clone();

        let escalations = read_crew_file(host, "escalations.md")?.unwrap_or_default();
        if print_file_delta("ESCALATION", &last_escalations, &escalations) {
            println!("ESCALATION | review with: herdr task policy {slug} [--allow DOMAIN]");
        }
        last_escalations = escalations;

        // Surface crew agent state transitions so a stalled or errored agent is
        // visible even when nobody updates status.md (e.g. a dead model).
        if let Ok(Ok(value)) = ssh_herdr_json(host, &["agent", "list"]) {
            let agents = value
                .pointer("/result/agents")
                .and_then(|value| value.as_array())
                .cloned()
                .unwrap_or_default();
            for entry in agents {
                let name = entry
                    .get("name")
                    .and_then(|value| value.as_str())
                    .unwrap_or("<unnamed>")
                    .to_string();
                let kind = entry
                    .get("agent")
                    .and_then(|value| value.as_str())
                    .unwrap_or("?");
                let state = entry
                    .get("agent_status")
                    .and_then(|value| value.as_str())
                    .unwrap_or("?")
                    .to_string();
                match agent_states.get(&name) {
                    Some(previous) if *previous == state => {}
                    Some(previous) => {
                        println!("agents | {name} ({kind}): {previous} -> {state}");
                        agent_states.insert(name, state);
                    }
                    None => {
                        println!("agents | {name} ({kind}): {state}");
                        agent_states.insert(name, state);
                    }
                }
            }
        }

        match parse_result(tasks::current_run_slice(&last_status)) {
            Some(TaskResult::Done) => {
                // `RESULT: DONE` is prose an agent wrote about itself. It opens
                // the acceptance check; it does not pass it.
                match accept_completed_run(host, workdir, roles)? {
                    Ok(evidence) => {
                        println!(
                            "task {slug}: RESULT: DONE — accepted at {} (qc {}, implementer {})",
                            evidence.commit, evidence.qc, evidence.implementer
                        );
                        println!("{}", tasks::format_qc_evidence(&evidence));
                        println!("review with: herdr task attach {slug}");
                        return Ok(0);
                    }
                    Err(err) => {
                        eprintln!("task {slug}: RESULT: DONE, but it is NOT accepted: {err}");
                        eprintln!(
                            "the sandbox commit is not reviewed work; inspect it with \
                             `herdr task attach {slug}` before trusting it"
                        );
                        return Ok(EXIT_UNACCEPTED);
                    }
                }
            }
            Some(TaskResult::Failed) => {
                eprintln!("task {slug}: RESULT: FAILED — inspect with: herdr task attach {slug}");
                return Ok(1);
            }
            Some(TaskResult::Blocked) => {
                eprintln!(
                    "task {slug}: RESULT: BLOCKED — the crew needs resources. Review \
                     escalations with `herdr task policy {slug}`, grant or deny, then resume \
                     with: herdr task goal {slug} \"resources updated, continue\""
                );
                return Ok(3);
            }
            None => {}
        }
        std::thread::sleep(WATCH_POLL_DELAY);
    }
}

fn crew_assignment(host: &str) -> std::io::Result<Option<RoleAssignment>> {
    let Some(content) = read_crew_file(host, "assignment")? else {
        return Ok(None);
    };
    match parse_roles(content.trim()) {
        Ok(roles) => Ok(Some(roles)),
        Err(err) => {
            eprintln!("invalid crew assignment in the sandbox: {err}");
            Ok(None)
        }
    }
}

fn crew_models(host: &str) -> Result<tasks::ModelPins, String> {
    let content = read_evidence_file(host, "models")
        .map_err(|err| err.to_string())??
        .ok_or("sandbox predates model pins (models file missing); recreate the task")?;
    tasks::parse_models(&content).map_err(|err| format!("invalid sandbox model pins: {err}"))
}

fn probe_model(host: &str, role: &str) -> Result<(), String> {
    let helper = format!("{CREW_DIR}/bin/crew_models.py");
    let output = ssh_capture(
        host,
        &tasks::remote_command(&["python3", &helper, "probe", "--role", role]),
    )
    .map_err(|err| err.to_string())?;
    if output.exit_code != 0 {
        return Err(format!("model verification failed (exit {}): {} {}; close the {role} pane and rerun, or recreate the task",
            output.exit_code, output.stdout.trim(), output.stderr.trim()));
    }
    println!("{}", output.stdout.trim());
    Ok(())
}

/// Make sure a named crew agent is running inside the sandbox, creating a tab
/// and starting the assigned agent kind when missing.
fn ensure_agent(
    host: &str,
    role: &str,
    roles: &RoleAssignment,
    workdir: &str,
) -> Result<(), String> {
    let pins = crew_models(host)?;
    let helper = format!("{CREW_DIR}/bin/crew_models.py");
    let helper_check = ssh_capture(host, &format!("test -r {}", shell_quote(&helper)))
        .map_err(|err| err.to_string())?;
    if helper_check.exit_code != 0 {
        return Err(
            "sandbox predates model pins (crew_models.py missing); recreate the task".into(),
        );
    }
    let Some(kind) = roles.kind_for(role) else {
        return Err(format!("no kind assigned for role {role}"));
    };
    let passthrough = tasks::agent_start_args(kind, &pins);
    if passthrough.is_empty() {
        return Err(format!("unsupported crew agent kind: {kind}"));
    }
    if ssh_herdr_json(host, &["agent", "get", role])
        .map_err(|err| err.to_string())?
        .is_ok()
    {
        dismiss_codex_update(host, role)?;
        let info =
            ssh_herdr_json(host, &["agent", "get", role]).map_err(|err| err.to_string())??;
        if info.pointer("/result/agent/agent").and_then(|v| v.as_str()) != Some(kind) {
            return Err(format!("{role} kind differs from assignment; close the {role} pane and rerun, or recreate the task"));
        }
        return match info
            .pointer("/result/agent/agent_status")
            .and_then(|v| v.as_str())
        {
            Some("working") => {
                eprintln!("warning: {role} model not verified (busy)");
                Ok(())
            }
            Some("idle" | "done") => probe_model(host, role),
            _ => Err(format!(
                "{role} is not ready for model verification; inspect its pane"
            )),
        };
    }

    println!("starting {role} ({kind})...");
    let workspaces =
        ssh_herdr_json(host, &["workspace", "list"]).map_err(|err| err.to_string())??;
    let workspace_id = workspaces
        .pointer("/result/workspaces/0/workspace_id")
        .and_then(|value| value.as_str());
    let created = if let Some(workspace_id) = workspace_id {
        ssh_herdr_json(
            host,
            &[
                "tab",
                "create",
                "--workspace",
                workspace_id,
                "--cwd",
                workdir,
                "--label",
                role,
                "--no-focus",
            ],
        )
    } else {
        // A fresh headless server has no workspace. Its first role uses the
        // root pane of a new workspace; subsequent roles get their own tabs.
        ssh_herdr_json(
            host,
            &[
                "workspace",
                "create",
                "--cwd",
                workdir,
                "--label",
                role,
                "--focus",
            ],
        )
    }
    .map_err(|err| err.to_string())??;
    let Some(pane_id) = created
        .pointer("/result/root_pane/pane_id")
        .and_then(|value| value.as_str())
        .map(str::to_string)
    else {
        return Err(format!("role pane creation returned no pane id: {created}"));
    };

    let timeout = AGENT_START_TIMEOUT_MS.to_string();
    let mut argv = vec![
        "herdr",
        "agent",
        "start",
        role,
        "--kind",
        kind,
        "--pane",
        &pane_id,
        "--timeout",
        &timeout,
        "--",
    ];
    argv.extend(passthrough.iter().map(String::as_str));
    let start_remote = tasks::remote_command(&argv);
    let output = ssh_capture(host, &start_remote).map_err(|err| err.to_string())?;
    if output.exit_code != 0 {
        return Err(format!(
            "agent start failed (exit {}): {}",
            output.exit_code,
            if output.stderr.trim().is_empty() {
                output.stdout.trim()
            } else {
                output.stderr.trim()
            }
        ));
    }
    probe_model(host, role)
}

/// Recover only the known updater menu in older crew images. Never press
/// confirmation keys for an arbitrary blocked agent or permission dialog.
fn dismiss_codex_update(host: &str, role: &str) -> Result<(), String> {
    let agent = ssh_herdr_json(host, &["agent", "get", role]).map_err(|err| err.to_string())??;
    let kind = agent
        .pointer("/result/agent/agent")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let state = agent
        .pointer("/result/agent/agent_status")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if kind != "codex" || state != "blocked" {
        return Ok(());
    }
    let remote = tasks::remote_command(&[
        "herdr",
        "agent",
        "read",
        role,
        "--source",
        "detection",
        "--format",
        "text",
    ]);
    let screen = ssh_capture(host, &remote).map_err(|err| err.to_string())?;
    if tasks::codex_startup_update_menu(kind, state, &screen.stdout) {
        let remote = tasks::remote_command(&["herdr", "agent", "send-keys", role, "down", "enter"]);
        let output = ssh_capture(host, &remote).map_err(|err| err.to_string())?;
        if output.exit_code != 0 {
            return Err(format!(
                "could not dismiss the Codex update menu: {}",
                output.stderr
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// task ls / status / attach / policy / rm
// ---------------------------------------------------------------------------

fn task_ls() -> std::io::Result<i32> {
    let output = Command::new("sbx")
        .arg("ls")
        .stdin(Stdio::null())
        .output()
        .map_err(|err| std::io::Error::other(format!("failed to run sbx: {err}")))?;
    if !output.status.success() {
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
        return Ok(output.status.code().unwrap_or(1));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut found = false;
    for line in stdout.lines() {
        if line.contains(tasks::TASK_SANDBOX_PREFIX) {
            println!("{line}");
            found = true;
        }
    }
    if !found {
        println!("no task sandboxes (create one with `herdr task new <slug>`)");
    }
    Ok(0)
}

fn task_status(args: &[String]) -> std::io::Result<i32> {
    let Some(slug) = args.first() else {
        eprintln!("usage: herdr task status <slug>");
        return Ok(2);
    };
    if let Err(err) = validate_slug(slug) {
        eprintln!("{err}");
        return Ok(2);
    }
    let host = ssh_host(slug);
    match ssh_herdr_json(&host, &["agent", "list"])? {
        Ok(value) => {
            let agents = value
                .pointer("/result/agents")
                .and_then(|value| value.as_array())
                .cloned()
                .unwrap_or_default();
            if agents.is_empty() {
                println!("no crew agents running (send work with `herdr task goal {slug} ...`)");
            } else {
                for agent in agents {
                    let name = agent
                        .get("name")
                        .and_then(|value| value.as_str())
                        .unwrap_or("<unnamed>");
                    let kind = agent
                        .get("agent")
                        .and_then(|value| value.as_str())
                        .unwrap_or("?");
                    let state = agent
                        .get("agent_status")
                        .and_then(|value| value.as_str())
                        .unwrap_or("?");
                    println!("{name}\t{kind}\t{state}");
                    if state == "blocked" {
                        let remote = tasks::remote_command(&[
                            "herdr",
                            "agent",
                            "read",
                            name,
                            "--source",
                            "recent-unwrapped",
                            "--format",
                            "text",
                        ]);
                        let screen = ssh_capture(&host, &remote)?;
                        if !screen.stdout.trim().is_empty() {
                            println!("--- blocked agent {name} ---\n{}", screen.stdout.trim());
                        }
                    }
                }
            }
        }
        Err(err) => {
            eprintln!("{err}");
            // Keep diagnosis inside the task lifecycle: a missing server can
            // leave the VM running while its crew has stopped making progress.
            let log = ssh_capture(
                &host,
                "tail -n 40 /tmp/herdr-server.log /home/agent/.config/herdr/herdr-server.log 2>/dev/null",
            )?;
            if !log.stdout.trim().is_empty() {
                eprintln!("sandbox server log:\n{}", log.stdout.trim());
            }
            return Ok(1);
        }
    }
    if let Some(status) = read_crew_file(&host, "status.md")? {
        println!("--- crew status ---");
        print!("{status}");
    }
    Ok(0)
}

fn task_attach(args: &[String]) -> std::io::Result<i32> {
    let Some(slug) = args.first() else {
        eprintln!("usage: herdr task attach <slug>");
        return Ok(2);
    };
    if let Err(err) = validate_slug(slug) {
        eprintln!("{err}");
        return Ok(2);
    }
    let current_exe = std::env::current_exe()?;
    let status = Command::new(current_exe)
        .arg("--remote")
        .arg(format!("ssh://{}", ssh_host(slug)))
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;
    Ok(status.code().unwrap_or(1))
}

fn task_policy(args: &[String]) -> std::io::Result<i32> {
    let Some(slug) = args.first() else {
        eprintln!("usage: herdr task policy <slug> [--allow DOMAIN]");
        return Ok(2);
    };
    if let Err(err) = validate_slug(slug) {
        eprintln!("{err}");
        return Ok(2);
    }
    if let Some(option) = args.get(1) {
        if option != "--allow" {
            eprintln!("unknown option: {option}");
            return Ok(2);
        }
        let Some(domain) = args.get(2) else {
            eprintln!("missing value for --allow");
            return Ok(2);
        };
        let argv: Vec<String> = ["sbx", "policy", "allow", "network", domain, "--sandbox"]
            .iter()
            .map(|arg| arg.to_string())
            .chain(std::iter::once(sandbox_name(slug)))
            .collect();
        return run_inherited(&argv).map(|code| if code == 0 { 0 } else { 1 });
    }

    match read_crew_file(&ssh_host(slug), "escalations.md")? {
        Some(content) => {
            println!("--- escalations ({slug}) ---");
            print!("{content}");
            println!("apply one with: herdr task policy {slug} --allow <domain>");
        }
        None => println!("no escalations recorded for task {slug}"),
    }
    Ok(0)
}

fn task_rm(args: &[String]) -> std::io::Result<i32> {
    let Some(slug) = args.first() else {
        eprintln!("usage: herdr task rm <slug>");
        return Ok(2);
    };
    if let Err(err) = validate_slug(slug) {
        eprintln!("{err}");
        return Ok(2);
    }
    let argv: Vec<String> = ["sbx", "rm"]
        .iter()
        .map(|arg| arg.to_string())
        .chain(std::iter::once(sandbox_name(slug)))
        .collect();
    run_inherited(&argv)
}
