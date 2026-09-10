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
    self, parse_result, parse_roles, rotation_for, sandbox_name, sbx_create_argv, shell_quote,
    ssh_host, validate_slug, RoleAssignment, TaskResult,
};

const CREW_DIR: &str = "/home/agent/crew";
const READY_POLL_ATTEMPTS: u32 = 36;
const READY_POLL_DELAY: Duration = Duration::from_secs(5);
const AGENT_START_TIMEOUT_MS: u64 = 240_000;
const WATCH_POLL_DELAY: Duration = Duration::from_secs(20);

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
        "  herdr task new <slug> [--dir PATH] [--kit REF] [--mixin REF]... [--roles orchestrator=K,planner=K,implementer=K,qc=K]"
    );
    eprintln!("  herdr task goal <slug> <text> [--no-watch]");
    eprintln!("  herdr task watch <slug>");
    eprintln!("  herdr task ls");
    eprintln!("  herdr task status <slug>");
    eprintln!("  herdr task attach <slug>");
    eprintln!("  herdr task policy <slug> [--allow DOMAIN]");
    eprintln!("  herdr task rm <slug>");
    eprintln!();
    eprintln!("Tasks run in Docker SBX sandboxes provisioned from the herdr-crew kit.");
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

// ---------------------------------------------------------------------------
// task new
// ---------------------------------------------------------------------------

fn task_new(args: &[String]) -> std::io::Result<i32> {
    let Some(slug) = args.first().cloned() else {
        eprintln!(
            "usage: herdr task new <slug> [--dir PATH] [--kit REF] [--mixin REF]... [--roles ...]"
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
    let mut mixins: Vec<String> = Vec::new();
    let mut index = 1;
    while index < args.len() {
        let (option, value) = (args[index].as_str(), args.get(index + 1));
        match option {
            "--dir" | "--kit" | "--roles" | "--mixin" => {
                let Some(value) = value else {
                    eprintln!("missing value for {option}");
                    return Ok(2);
                };
                match option {
                    "--dir" => dir = Some(value.clone()),
                    "--kit" => kit = Some(value.clone()),
                    "--mixin" => mixins.push(value.clone()),
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

    let argv = sbx_create_argv(&kit, &workspace, &slug, &roles, &mixins);
    let exit_code = run_inherited(&argv)?;
    if exit_code != 0 {
        eprintln!("sbx create failed (exit {exit_code})");
        return Ok(1);
    }

    println!("waiting for the herdr server inside the sandbox...");
    let host = ssh_host(&slug);
    for attempt in 1..=READY_POLL_ATTEMPTS {
        // `herdr status server` exits 0 whether or not a server is running, so
        // readiness must check the reported status text, not the exit code.
        let probe = server_running_probe(&host)?;
        if probe.exit_code == 0 {
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

/// Idempotent remote start of the headless server (safe to run repeatedly).
const START_SERVER_REMOTE: &str =
    "pgrep -f 'herdr server' >/dev/null 2>&1 || (nohup herdr server >>/tmp/herdr-server.log 2>&1 &)";

fn server_running_probe(host: &str) -> std::io::Result<RemoteOutput> {
    ssh_capture(
        host,
        "herdr status server 2>/dev/null | grep -q 'status: running'",
    )
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
    let mut positionals = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--no-watch" => watch = false,
            other if other.starts_with("--") => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
            _ => positionals.push(arg.clone()),
        }
    }
    let (Some(slug), Some(goal)) = (positionals.first().cloned(), positionals.get(1).cloned())
    else {
        eprintln!("usage: herdr task goal <slug> <text> [--no-watch]");
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
        .unwrap_or_else(|| "/home/agent/workspace".to_string());

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

    // Scope this run: RESULT lines before the marker belong to previous goals
    // and must not terminate this run's watch.
    let marker = format!(
        "printf '\\n%s\\n' {} >> {}",
        shell_quote(tasks::GOAL_MARKER),
        shell_quote(&format!("{CREW_DIR}/status.md"))
    );
    let _ = ssh_capture(&host, &marker)?;

    let goal_prompt = format!(
        "You are the crew orchestrator for this task. Read {CREW_DIR}/roles/orchestrator.md \
         and follow it exactly: assemble the crew from {CREW_DIR}/assignment, coordinate \
         plan -> implement -> qc rounds until qc passes, record progress in \
         {CREW_DIR}/status.md, and finish by appending a final line \
         'RESULT: DONE', 'RESULT: FAILED', or 'RESULT: BLOCKED' to {CREW_DIR}/status.md. \
         The workspace is {workdir}. /goal: {goal}"
    );
    if let Err(err) = ssh_herdr_json(&host, &["agent", "prompt", "orchestrator", &goal_prompt])? {
        eprintln!("failed to deliver the goal: {err}");
        return Ok(1);
    }
    println!("goal delivered to the sandboxed orchestrator.");
    if !watch {
        println!("follow progress with: herdr task watch {slug}");
        return Ok(0);
    }
    watch_task(&slug, &host)
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
    watch_task(slug, &ssh_host(slug))
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

fn watch_task(slug: &str, host: &str) -> std::io::Result<i32> {
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
                println!("task {slug}: RESULT: DONE — review with: herdr task attach {slug}");
                return Ok(0);
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

/// Make sure a named crew agent is running inside the sandbox, creating a tab
/// and starting the assigned agent kind when missing.
fn ensure_agent(
    host: &str,
    role: &str,
    roles: &RoleAssignment,
    workdir: &str,
) -> Result<(), String> {
    let exists = ssh_herdr_json(host, &["agent", "get", role])
        .map_err(|err| err.to_string())?
        .is_ok();
    if exists {
        return Ok(());
    }

    let Some(kind) = roles.kind_for(role) else {
        return Err(format!("no kind assigned for role {role}"));
    };
    println!("starting {role} ({kind})...");
    let tab = ssh_herdr_json(
        host,
        &[
            "tab",
            "create",
            "--cwd",
            workdir,
            "--label",
            role,
            "--no-focus",
        ],
    )
    .map_err(|err| err.to_string())??;
    let Some(pane_id) = tab
        .pointer("/result/root_pane/pane_id")
        .and_then(|value| value.as_str())
        .map(str::to_string)
    else {
        return Err(format!("tab create returned no pane id: {tab}"));
    };

    let timeout = AGENT_START_TIMEOUT_MS.to_string();
    let mut start_args = vec![
        "agent",
        "start",
        role,
        "--kind",
        kind,
        "--pane",
        &pane_id,
        "--timeout",
        &timeout,
    ];
    // pi is the Google-models member and pins the crew model configured by the
    // kit. The variable expands in the sandbox shell, so it stays outside the
    // quoted argv.
    let start_remote = if kind == "pi" {
        let mut argv = vec!["herdr"];
        argv.extend_from_slice(&start_args);
        format!(
            "{} -- --provider google --model \"${{HERDR_CREW_GEMINI_MODEL:-gemini-3.8-flash}}\"",
            tasks::remote_command(&argv)
        )
    } else {
        let mut argv = vec!["herdr"];
        argv.append(&mut start_args);
        tasks::remote_command(&argv)
    };
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
                }
            }
        }
        Err(err) => {
            eprintln!("{err}");
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
