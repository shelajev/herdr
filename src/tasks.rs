//! Task crews: host-side lifecycle for Docker SBX sandboxes running the
//! herdr-crew kit (a nested herdr server plus an orchestrator, planner,
//! implementer, and qc agent).
//!
//! The goal-holding orchestrator runs *inside* the sandbox; the host herdr
//! only provisions sandboxes, delivers goals, watches the crew status file,
//! and arbitrates resource escalations. Pure argv/name/parse logic lives here
//! so it is testable without sandboxes or processes; the CLI flow is in
//! `crate::cli::task`.

pub(crate) const TASK_SANDBOX_PREFIX: &str = "herdr-task-";
pub(crate) const DEFAULT_KIT: &str = "docker.io/olegselajev241/herdr-crew-kit:latest";
pub(crate) const KIT_ENV_VAR: &str = "HERDR_TASK_KIT";

/// Crew role assignment for one task. Each role names an agent kind.
///
/// The orchestrator holds the goal and coordinates the crew from *inside* the
/// sandbox — goal-seeking behavior stays contained; the host only delivers the
/// goal and arbitrates resources.
///
/// Invariant: `implementer != qc` — quality control is never performed by the
/// model that implemented the work. `parse_roles` enforces it for explicit
/// assignments and `rotation_for` produces only assignments that satisfy it.
/// The orchestrator may share a product with any role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RoleAssignment {
    pub orchestrator: String,
    pub planner: String,
    pub implementer: String,
    pub qc: String,
}

impl RoleAssignment {
    pub(crate) fn kit_arg(&self) -> String {
        format!(
            "orchestrator={},planner={},implementer={},qc={}",
            self.orchestrator, self.planner, self.implementer, self.qc
        )
    }

    pub(crate) fn kind_for(&self, role: &str) -> Option<&str> {
        match role {
            "orchestrator" => Some(self.orchestrator.as_str()),
            "planner" => Some(self.planner.as_str()),
            "implementer" => Some(self.implementer.as_str()),
            "qc" => Some(self.qc.as_str()),
            _ => None,
        }
    }
}

/// The three products rotated across roles. All six permutations of the
/// planner/implementer/qc triple assign each product exactly one role, so
/// implementer != qc holds by construction; the orchestrator independently
/// takes one of the three products (18 assignments total). `pi` (pi.dev) is
/// the Google-models member: provider google, GEMINI_API_KEY, model pinned by
/// the kit's `gemini_model` argument.
const CREW_KINDS: [&str; 3] = ["claude", "codex", "pi"];
const ROTATIONS: [[usize; 3]; 6] = [
    [0, 1, 2],
    [0, 2, 1],
    [1, 0, 2],
    [1, 2, 0],
    [2, 0, 1],
    [2, 1, 0],
];

pub(crate) fn validate_slug(slug: &str) -> Result<(), String> {
    if slug.is_empty() || slug.len() > 40 {
        return Err("task slug must be 1-40 characters".to_string());
    }
    if !slug
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(format!(
            "task slug may only contain lowercase letters, digits, and '-': {slug}"
        ));
    }
    if slug.starts_with('-') || slug.ends_with('-') {
        return Err("task slug may not start or end with '-'".to_string());
    }
    Ok(())
}

pub(crate) fn sandbox_name(slug: &str) -> String {
    format!("{TASK_SANDBOX_PREFIX}{slug}")
}

/// SSH host for a task sandbox, served by `sbx setup ssh`'s managed
/// `Host *.sbx` block.
pub(crate) fn ssh_host(slug: &str) -> String {
    format!("{}.sbx", sandbox_name(slug))
}

/// Deterministic role rotation: the slug picks one of the six permutations of
/// the crew products over (planner, implementer, qc) plus an orchestrator
/// product. Rotating per task balances subscription usage; determinism keeps a
/// task's assignment reproducible from its slug alone.
pub(crate) fn rotation_for(slug: &str) -> RoleAssignment {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in slug.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let rotation = ROTATIONS[(hash % ROTATIONS.len() as u64) as usize];
    let orchestrator =
        CREW_KINDS[((hash / ROTATIONS.len() as u64) % CREW_KINDS.len() as u64) as usize];
    RoleAssignment {
        orchestrator: orchestrator.to_string(),
        planner: CREW_KINDS[rotation[0]].to_string(),
        implementer: CREW_KINDS[rotation[1]].to_string(),
        qc: CREW_KINDS[rotation[2]].to_string(),
    }
}

/// Parse an explicit `orchestrator=KIND,planner=KIND,implementer=KIND,qc=KIND`
/// assignment.
pub(crate) fn parse_roles(value: &str) -> Result<RoleAssignment, String> {
    let mut orchestrator = None;
    let mut planner = None;
    let mut implementer = None;
    let mut qc = None;
    for pair in value.split(',') {
        let Some((role, kind)) = pair.split_once('=') else {
            return Err(format!("invalid role assignment (want role=kind): {pair}"));
        };
        let kind = kind.trim();
        if kind.is_empty() || !kind.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
            return Err(format!("invalid agent kind for {role}: {kind}"));
        }
        let slot = match role.trim() {
            "orchestrator" => &mut orchestrator,
            "planner" => &mut planner,
            "implementer" => &mut implementer,
            "qc" => &mut qc,
            other => return Err(format!("unknown crew role: {other}")),
        };
        if slot.replace(kind.to_string()).is_some() {
            return Err(format!("duplicate role: {role}"));
        }
    }
    let (Some(orchestrator), Some(planner), Some(implementer), Some(qc)) =
        (orchestrator, planner, implementer, qc)
    else {
        return Err("roles must assign orchestrator, planner, implementer, and qc".to_string());
    };
    if implementer == qc {
        return Err(format!(
            "qc must not be the same model as the implementer (both {qc}); \
             quality control requires a different model"
        ));
    }
    Ok(RoleAssignment {
        orchestrator,
        planner,
        implementer,
        qc,
    })
}

/// `sbx create` argv provisioning a task sandbox from the herdr-crew kit.
/// Each mixin becomes an sbx `--kit` flag, stacking extra tools, network
/// rules, and agent memory onto the crew sandbox at creation.
pub(crate) fn sbx_create_argv(
    kit: &str,
    workspace_dir: &str,
    slug: &str,
    roles: &RoleAssignment,
    mixins: &[String],
) -> Vec<String> {
    let mut argv = vec![
        "sbx".to_string(),
        "create".to_string(),
        kit.to_string(),
        workspace_dir.to_string(),
        "--name".to_string(),
        sandbox_name(slug),
        "--kit-arg".to_string(),
        format!("roles={}", roles.kit_arg()),
    ];
    for mixin in mixins {
        argv.push("--kit".to_string());
        argv.push(mixin.clone());
    }
    argv
}

/// Quote one argument for the remote shell command line ssh assembles.
pub(crate) fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value.chars().all(|ch| {
            ch.is_ascii_alphanumeric()
                || matches!(
                    ch,
                    '@' | '%' | '_' | '+' | '=' | ':' | ',' | '.' | '/' | '-'
                )
        })
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Join argv into one remote shell command line, quoting each argument.
pub(crate) fn remote_command(argv: &[&str]) -> String {
    argv.iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Host-written marker appended to the crew status file each time a goal is
/// delivered. RESULT lines are only honored after the newest marker, so a
/// previous run's `RESULT: DONE` cannot terminate a new run's watch.
pub(crate) const GOAL_MARKER: &str = "==== goal delivered ====";

/// The portion of the status file belonging to the current run: everything
/// after the last goal marker, or the whole file when no marker exists yet
/// (pre-marker sandboxes).
pub(crate) fn current_run_slice(status: &str) -> &str {
    match status.rfind(GOAL_MARKER) {
        Some(index) => &status[index..],
        None => status,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskResult {
    Done,
    Blocked,
    Failed,
}

/// Read the latest `RESULT: DONE|BLOCKED|FAILED` line the sandboxed
/// orchestrator wrote to the crew status file. Later lines win so an
/// orchestrator that resumes after being unblocked supersedes its earlier
/// result.
pub(crate) fn parse_result(status: &str) -> Option<TaskResult> {
    let mut result = None;
    for line in status.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("RESULT:") else {
            continue;
        };
        match rest.trim() {
            "DONE" => result = Some(TaskResult::Done),
            "BLOCKED" => result = Some(TaskResult::Blocked),
            "FAILED" => result = Some(TaskResult::Failed),
            _ => {}
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_validation_accepts_kebab_and_rejects_others() {
        assert!(validate_slug("fix-login-42").is_ok());
        for bad in ["", "Fix", "a b", "-lead", "trail-", "under_score"] {
            assert!(validate_slug(bad).is_err(), "expected rejection: {bad}");
        }
    }

    #[test]
    fn sandbox_and_ssh_names_are_derived_from_slug() {
        assert_eq!(sandbox_name("demo"), "herdr-task-demo");
        assert_eq!(ssh_host("demo"), "herdr-task-demo.sbx");
    }

    #[test]
    fn rotation_is_deterministic_and_never_lets_implementer_qc_itself() {
        for slug in ["a", "fix-login", "task-1", "zz", "herdr", "crew-42"] {
            let first = rotation_for(slug);
            assert_eq!(first, rotation_for(slug), "unstable rotation for {slug}");
            assert_ne!(
                first.implementer, first.qc,
                "implementer must differ from qc for {slug}"
            );
            assert!(
                CREW_KINDS.contains(&first.orchestrator.as_str()),
                "orchestrator must be a crew product: {slug}"
            );
            let mut kinds = [
                first.planner.as_str(),
                first.implementer.as_str(),
                first.qc.as_str(),
            ];
            kinds.sort_unstable();
            assert_eq!(
                kinds, CREW_KINDS,
                "each product holds one crew role: {slug}"
            );
        }
    }

    #[test]
    fn rotation_varies_the_orchestrator_across_slugs() {
        let distinct: std::collections::HashSet<String> =
            ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"]
                .iter()
                .map(|slug| rotation_for(slug).orchestrator)
                .collect();
        assert!(distinct.len() > 1, "orchestrator never rotates");
    }

    #[test]
    fn rotation_varies_across_slugs() {
        let distinct: std::collections::HashSet<String> = ["a", "b", "c", "d", "e", "f", "g", "h"]
            .iter()
            .map(|slug| rotation_for(slug).kit_arg())
            .collect();
        assert!(distinct.len() > 1, "rotation never varies");
    }

    #[test]
    fn parse_roles_round_trips_and_enforces_qc_independence() {
        let roles = parse_roles("orchestrator=claude,planner=codex,implementer=claude,qc=gemini")
            .expect("valid roles should parse");
        assert_eq!(
            roles.kit_arg(),
            "orchestrator=claude,planner=codex,implementer=claude,qc=gemini"
        );
        assert_eq!(roles.kind_for("qc"), Some("gemini"));
        assert_eq!(roles.kind_for("orchestrator"), Some("claude"));

        let same = parse_roles("orchestrator=codex,planner=codex,implementer=claude,qc=claude");
        assert!(same.is_err(), "qc == implementer must be rejected");
        assert!(
            parse_roles("planner=codex,implementer=claude,qc=gemini").is_err(),
            "missing orchestrator must be rejected"
        );
        assert!(
            parse_roles("orchestrator=a,planner=codex,planner=claude,implementer=a,qc=b").is_err()
        );
        assert!(parse_roles("captain=claude,implementer=a,qc=b").is_err());
    }

    #[test]
    fn sbx_create_argv_provisions_named_sandbox_with_roles_and_mixins() {
        let roles = parse_roles("orchestrator=gemini,planner=gemini,implementer=codex,qc=claude")
            .expect("valid roles should parse");
        let argv = sbx_create_argv("./kits/herdr-crew/", "/work/repo", "demo", &roles, &[]);
        assert_eq!(
            argv,
            [
                "sbx",
                "create",
                "./kits/herdr-crew/",
                "/work/repo",
                "--name",
                "herdr-task-demo",
                "--kit-arg",
                "roles=orchestrator=gemini,planner=gemini,implementer=codex,qc=claude",
            ]
        );

        let mixins = vec![
            "git+https://github.com/shelajev/yt-transcript-sbx-kit.git".to_string(),
            "docker.io/example/other-mixin:1.0".to_string(),
        ];
        let argv = sbx_create_argv("./kits/herdr-crew/", "/work/repo", "demo", &roles, &mixins);
        assert_eq!(
            argv[8..],
            [
                "--kit",
                "git+https://github.com/shelajev/yt-transcript-sbx-kit.git",
                "--kit",
                "docker.io/example/other-mixin:1.0",
            ]
        );
    }

    #[test]
    fn remote_command_quotes_unsafe_arguments() {
        assert_eq!(
            remote_command(&["herdr", "agent", "prompt", "planner", "do it; rm -rf /"]),
            "herdr agent prompt planner 'do it; rm -rf /'"
        );
        assert_eq!(remote_command(&["echo", "it's"]), "echo 'it'\\''s'");
    }

    #[test]
    fn results_are_scoped_to_the_current_run() {
        let status = format!("old work\nRESULT: DONE\n{GOAL_MARKER}\nnew work in progress\n");
        assert_eq!(parse_result(current_run_slice(&status)), None);
        let status = format!("old work\nRESULT: DONE\n{GOAL_MARKER}\nnew work\nRESULT: FAILED\n");
        assert_eq!(
            parse_result(current_run_slice(&status)),
            Some(TaskResult::Failed)
        );
        // No marker (pre-marker sandbox): whole file counts.
        assert_eq!(
            parse_result(current_run_slice("RESULT: DONE")),
            Some(TaskResult::Done)
        );
    }

    #[test]
    fn result_parsing_takes_the_latest_line() {
        assert_eq!(parse_result(""), None);
        assert_eq!(parse_result("notes only"), None);
        assert_eq!(
            parse_result("RESULT: BLOCKED\nunblocked, resuming\nRESULT: DONE\n"),
            Some(TaskResult::Done)
        );
        assert_eq!(
            parse_result("progress\n  RESULT: FAILED"),
            Some(TaskResult::Failed)
        );
        assert_eq!(parse_result("RESULT: MAYBE"), None);
        assert_eq!(parse_result("RESULT: BLOCKED"), Some(TaskResult::Blocked));
    }
}
