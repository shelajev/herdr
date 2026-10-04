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

/// Sandbox-owned pins. The kit's models file is the only source of defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelPins {
    pub claude: String,
    pub codex: String,
    pub pi_provider: String,
    pub pi_model: String,
}

impl ModelPins {
    pub(crate) fn file_value(&self) -> String {
        format!(
            "claude={},codex={},pi={}/{}",
            self.claude, self.codex, self.pi_provider, self.pi_model
        )
    }
}

pub(crate) fn validate_model_value(value: &str) -> Result<(), String> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:/-".contains(&b))
    {
        return Err("model values must match [A-Za-z0-9._:/-]+".into());
    }
    Ok(())
}

pub(crate) fn parse_models(value: &str) -> Result<ModelPins, String> {
    let (mut claude, mut codex, mut pi) = (None, None, None);
    for pair in value.trim_end_matches(['\r', '\n']).split(',') {
        let (kind, model) = pair
            .split_once('=')
            .ok_or("models must contain kind=model pairs")?;
        validate_model_value(model)?;
        let slot = match kind {
            "claude" => &mut claude,
            "codex" => &mut codex,
            "pi" => &mut pi,
            _ => return Err(format!("unknown model kind: {kind}")),
        };
        if slot.replace(model.to_string()).is_some() {
            return Err(format!("duplicate model kind: {kind}"));
        }
    }
    let (Some(claude), Some(codex), Some(pi)) = (claude, codex, pi) else {
        return Err("models must assign claude, codex, and pi".into());
    };
    let (provider, model) = pi
        .split_once('/')
        .ok_or("pi model must include provider/model")?;
    validate_model_value(provider)?;
    validate_model_value(model)?;
    Ok(ModelPins {
        claude,
        codex,
        pi_provider: provider.into(),
        pi_model: model.into(),
    })
}

pub(crate) fn agent_start_args(kind: &str, pins: &ModelPins) -> Vec<String> {
    match kind {
        "claude" => vec!["--model".into(), pins.claude.clone()],
        "codex" => vec![
            "-m".into(),
            pins.codex.clone(),
            "-c".into(),
            "check_for_update_on_startup=false".into(),
        ],
        "pi" => vec![
            "--provider".into(),
            pins.pi_provider.clone(),
            "--model".into(),
            pins.pi_model.clone(),
        ],
        _ => Vec::new(), // The caller refuses unsupported crew kinds before starting.
    }
}

/// Creation overrides stay separate from role assignment and contain no defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ModelOverrides {
    pub claude: Option<String>,
    pub codex: Option<String>,
    pub pi_provider: Option<String>,
    pub pi_model: Option<String>,
}

impl ModelOverrides {
    pub(crate) fn with_fallback(self, fallback: Self) -> Self {
        Self {
            claude: self.claude.or(fallback.claude),
            codex: self.codex.or(fallback.codex),
            pi_provider: self.pi_provider.or(fallback.pi_provider),
            pi_model: self.pi_model.or(fallback.pi_model),
        }
    }

    pub(crate) fn kit_args(&self) -> Result<Vec<String>, String> {
        let mut args = Vec::new();
        for (name, value) in [
            ("claude_model", &self.claude),
            ("codex_model", &self.codex),
            ("pi_provider", &self.pi_provider),
            ("gemini_model", &self.pi_model),
        ] {
            if let Some(value) = value {
                validate_model_value(value)?;
                if name == "pi_provider" && value.contains('/') {
                    return Err("pi provider cannot contain '/'".into());
                }
                args.extend(["--kit-arg".into(), format!("{name}={value}")]);
            }
        }
        Ok(args)
    }
}

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
    model_args: &[String],
) -> Vec<String> {
    let mut argv = vec![
        "sbx".to_string(),
        "create".to_string(),
        kit.to_string(),
        workspace_dir.to_string(),
        "--name".to_string(),
        sandbox_name(slug),
        "--skills".to_string(),
        "off".to_string(),
        "--kit-arg".to_string(),
        format!("roles={}", roles.kit_arg()),
    ];
    argv.extend_from_slice(model_args);
    for mixin in mixins {
        argv.push("--kit".to_string());
        argv.push(mixin.clone());
    }
    argv
}

/// Recognize the current, selected Codex updater without acting on old history
/// or unrelated confirmation dialogs. Callers supply the detection snapshot.
pub(crate) fn codex_startup_update_menu(kind: &str, state: &str, screen: &str) -> bool {
    kind == "codex"
        && state == "blocked"
        && screen.contains("Update available!")
        && screen.contains("› 1. Update now")
        && screen.contains("2. Skip")
        && screen.contains("Press enter to continue")
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

// ---------------------------------------------------------------------------
// Completion evidence: a run is only accepted on machine-readable QC evidence
// ---------------------------------------------------------------------------
//
// `RESULT: DONE` in status.md is prose written by an agent. It is kept for
// readability and for the FAILED/BLOCKED outcomes, but it can no longer, on
// its own, make `herdr task watch` exit 0. Success additionally requires a
// QC report that names the exact reviewed commit of the current run.
//
// The evidence lives in two JSON files under the crew directory, outside the
// reviewed repository, so it never lands in a commit and never touches the
// frozen client endpoint contract:
//
// * `run.json` — written by the *host* immediately before a goal is delivered.
//   It mints a fresh run id, so evidence from an earlier run cannot be reused.
// * `qc.json`  — written by the *QC agent* after it actually ran the checks.
//
// Everything here is pure parsing and comparison so it is testable without a
// sandbox, SSH, or a process.

/// Evidence schema version understood by this build. An artifact declaring any
/// other version is rejected as unsupported rather than best-effort parsed: a
/// newer coordinator must not be able to talk an older host into accepting a
/// report whose meaning it does not know.
pub(crate) const EVIDENCE_VERSION: u64 = 1;

/// Crew-directory file names for the two evidence artifacts.
pub(crate) const RUN_METADATA_FILE: &str = "run.json";
pub(crate) const QC_EVIDENCE_FILE: &str = "qc.json";

/// Host-minted identity for one goal delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunMetadata {
    pub run_id: String,
    pub goal_digest: String,
    pub workspace: String,
}

impl RunMetadata {
    /// Serialize for the crew directory. Written by the host before prompting.
    pub(crate) fn to_json(&self) -> String {
        format!(
            "{{\n  \"version\": {},\n  \"run_id\": {},\n  \"goal_digest\": {},\n  \"workspace\": {}\n}}\n",
            EVIDENCE_VERSION,
            json_string(&self.run_id),
            json_string(&self.goal_digest),
            json_string(&self.workspace),
        )
    }

    pub(crate) fn parse(raw: &str) -> Result<Self, EvidenceError> {
        let value: serde_json::Value =
            serde_json::from_str(raw).map_err(|err| EvidenceError::Malformed(err.to_string()))?;
        check_version(&value)?;
        Ok(Self {
            run_id: required_str(&value, "run_id")?,
            goal_digest: required_str(&value, "goal_digest")?,
            workspace: required_str(&value, "workspace")?,
        })
    }
}

/// Mint run metadata for a goal delivery. The run id embeds a wall-clock
/// nanosecond stamp and a digest of the goal and workspace, so two deliveries
/// never share an id even for an identical goal.
pub(crate) fn new_run_metadata(goal: &str, workspace: &str) -> RunMetadata {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let goal_digest = digest(goal);
    let run_id = format!(
        "{nanos:032x}-{}",
        &digest(&format!("{nanos}{workspace}{goal}"))[..16]
    );
    RunMetadata {
        run_id,
        goal_digest,
        workspace: workspace.to_string(),
    }
}

/// Lowercase hex SHA-256 of `value`.
pub(crate) fn digest(value: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// One check a QC agent claims to have executed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QcCheck {
    pub name: String,
    pub command: String,
    pub outcome: String,
    pub exit_code: Option<i64>,
}

impl QcCheck {
    fn succeeded(&self) -> bool {
        self.outcome == "passed" && self.exit_code == Some(0)
    }

    /// Whether this entry actually says what was run. A check with a blank name
    /// or a blank command describes nothing, so it cannot be the evidence that
    /// something was verified — however green its status looks.
    fn is_described(&self) -> bool {
        !self.name.trim().is_empty() && !self.command.trim().is_empty()
    }
}

/// A QC report about one exact commit of one exact run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QcEvidence {
    pub run_id: String,
    pub goal_digest: String,
    pub workspace: String,
    pub commit: String,
    pub verdict: String,
    pub implementer: String,
    pub qc: String,
    pub checks: Vec<QcCheck>,
}

/// Why a DONE could not be accepted. Every variant is a refusal: there is no
/// "probably fine" path, and a missing artifact is never success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EvidenceError {
    /// No run metadata or no QC report. Also what an older kit produces, since
    /// it never writes either file.
    Missing(&'static str),
    /// Present but not parseable, or missing a required field.
    Malformed(String),
    /// A version this build does not implement.
    UnsupportedVersion(u64),
    /// Evidence belongs to a different run, goal, or workspace.
    Stale(String),
    /// Evidence names a commit that is not the current reviewed HEAD.
    CommitMismatch { reported: String, actual: String },
    /// The workspace has tracked changes, so no commit describes its state.
    DirtyWorktree(String),
    /// QC did not pass, or did not actually run what acceptance requires.
    NotAccepted(String),
}

impl std::fmt::Display for EvidenceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(file) => write!(
                formatter,
                "the crew did not write {file}: this sandbox predates exact-commit QC evidence \
                 (schema v{EVIDENCE_VERSION}). Recreate the task with a current kit, or verify \
                 and accept the work by hand; `RESULT: DONE` alone is not acceptance"
            ),
            Self::Malformed(detail) => write!(formatter, "QC evidence is malformed: {detail}"),
            Self::UnsupportedVersion(version) => write!(
                formatter,
                "QC evidence declares schema version {version}, but this herdr implements \
                 v{EVIDENCE_VERSION}; refusing to guess what it means"
            ),
            Self::Stale(detail) => write!(formatter, "QC evidence is stale: {detail}"),
            Self::CommitMismatch { reported, actual } => write!(
                formatter,
                "QC reviewed {reported} but the workspace is now at {actual}; the implementation \
                 moved after review, so a new QC round is required"
            ),
            Self::DirtyWorktree(detail) => write!(
                formatter,
                "the workspace has uncommitted tracked changes, so no commit describes what QC \
                 reviewed: {detail}"
            ),
            Self::NotAccepted(detail) => write!(formatter, "QC did not accept this run: {detail}"),
        }
    }
}

impl QcEvidence {
    pub(crate) fn parse(raw: &str) -> Result<Self, EvidenceError> {
        let value: serde_json::Value =
            serde_json::from_str(raw).map_err(|err| EvidenceError::Malformed(err.to_string()))?;
        check_version(&value)?;
        let checks_value = value
            .get("checks")
            .and_then(|checks| checks.as_array())
            .ok_or_else(|| EvidenceError::Malformed("checks must be an array".to_string()))?;
        let mut checks = Vec::with_capacity(checks_value.len());
        for entry in checks_value {
            checks.push(QcCheck {
                name: required_str(entry, "name")?,
                command: required_str(entry, "command")?,
                outcome: required_str(entry, "outcome")?,
                exit_code: entry.get("exit_code").and_then(|code| code.as_i64()),
            });
        }
        Ok(Self {
            run_id: required_str(&value, "run_id")?,
            goal_digest: required_str(&value, "goal_digest")?,
            workspace: required_str(&value, "workspace")?,
            commit: required_str(&value, "commit")?,
            verdict: required_str(&value, "verdict")?,
            implementer: required_str(&value, "implementer")?,
            qc: required_str(&value, "qc")?,
            checks,
        })
    }
}

/// Decide whether a `RESULT: DONE` may be accepted.
///
/// The gate is deliberately generic: a task sandbox runs whatever repository
/// and goal the operator gives it, so this cannot require a particular build
/// or test command. What it requires is that the report belongs to the current
/// run, names the exact current clean commit, comes from the *configured* QC
/// role rather than the implementer, and lists checks that actually ran and
/// actually succeeded. Which checks are appropriate is the crew's judgement;
/// claiming success without any, or alongside a failure, is not.
///
/// `head` is the workspace's current full commit id and `porcelain` is the
/// output of `git status --porcelain` read from the same sandbox. Both are
/// re-read around the evidence read by the caller, so a tree that changes
/// mid-poll fails rather than races through.
pub(crate) fn accept_run(
    roles: &RoleAssignment,
    run_raw: Option<&str>,
    qc_raw: Option<&str>,
    head: &str,
    porcelain: &str,
) -> Result<QcEvidence, EvidenceError> {
    let run = RunMetadata::parse(run_raw.ok_or(EvidenceError::Missing(RUN_METADATA_FILE))?)?;
    let evidence = QcEvidence::parse(qc_raw.ok_or(EvidenceError::Missing(QC_EVIDENCE_FILE))?)?;

    if evidence.run_id != run.run_id {
        return Err(EvidenceError::Stale(format!(
            "report is for run {} but the current run is {}",
            evidence.run_id, run.run_id
        )));
    }
    if evidence.goal_digest != run.goal_digest {
        return Err(EvidenceError::Stale(
            "report is for a different goal than the one delivered to this run".to_string(),
        ));
    }
    if evidence.workspace != run.workspace {
        return Err(EvidenceError::Stale(format!(
            "report is for workspace {} but this run's workspace is {}",
            evidence.workspace, run.workspace
        )));
    }

    // A clean tree is a necessary condition, not a proof that nothing was
    // edited between QC and now; the run id and commit comparison carry that.
    if !porcelain.trim().is_empty() {
        return Err(EvidenceError::DirtyWorktree(
            porcelain
                .trim()
                .lines()
                .take(5)
                .collect::<Vec<_>>()
                .join("; "),
        ));
    }

    let head = head.trim();
    if !is_full_commit_id(head) {
        return Err(EvidenceError::Malformed(format!(
            "could not read a full commit id from the workspace (got {head:?})"
        )));
    }
    if !is_full_commit_id(&evidence.commit) {
        return Err(EvidenceError::Malformed(format!(
            "QC reported commit {:?}, which is not a full 40-character commit id",
            evidence.commit
        )));
    }
    if evidence.commit != head {
        return Err(EvidenceError::CommitMismatch {
            reported: evidence.commit.clone(),
            actual: head.to_string(),
        });
    }

    if evidence.verdict != "PASS" {
        return Err(EvidenceError::NotAccepted(format!(
            "verdict is {}",
            evidence.verdict
        )));
    }
    // Identity is checked against the crew's configured assignment, not merely
    // for being two different strings: a report may not invent a reviewer, and
    // the implementer may not sign off on itself under another label.
    if evidence.implementer != roles.implementer {
        return Err(EvidenceError::NotAccepted(format!(
            "the report names {} as the implementer, but this task is configured with {}",
            evidence.implementer, roles.implementer
        )));
    }
    if evidence.qc != roles.qc {
        return Err(EvidenceError::NotAccepted(format!(
            "the report names {} as QC, but this task is configured with {}",
            evidence.qc, roles.qc
        )));
    }
    if evidence.implementer == evidence.qc {
        return Err(EvidenceError::NotAccepted(format!(
            "{} reviewed its own work; quality control requires a different model",
            evidence.qc
        )));
    }

    // Which checks suit the goal is the crew's call; that some ran and all of
    // them succeeded is not.
    if evidence.checks.is_empty() {
        return Err(EvidenceError::NotAccepted(
            "a PASS with no executed checks is not evidence".to_string(),
        ));
    }
    for check in &evidence.checks {
        if !check.is_described() {
            return Err(EvidenceError::NotAccepted(format!(
                "a check entry must name what ran and the command that ran it; got name {:?} \
                 and command {:?}",
                check.name, check.command
            )));
        }
        if !check.succeeded() {
            return Err(EvidenceError::NotAccepted(format!(
                "check {:?} reported outcome {:?} (exit {:?}); a PASS may not carry a check that \
                 failed, was blocked, or was skipped",
                check.name, check.outcome, check.exit_code
            )));
        }
    }
    Ok(evidence)
}

/// A full git commit id: exactly 40 lowercase hex digits. Abbreviated ids are
/// rejected so a report cannot name a prefix that matches several commits.
pub(crate) fn is_full_commit_id(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn check_version(value: &serde_json::Value) -> Result<(), EvidenceError> {
    match value.get("version").and_then(|version| version.as_u64()) {
        Some(EVIDENCE_VERSION) => Ok(()),
        Some(other) => Err(EvidenceError::UnsupportedVersion(other)),
        None => Err(EvidenceError::Malformed(
            "missing an integer \"version\" field".to_string(),
        )),
    }
}

fn required_str(value: &serde_json::Value, field: &str) -> Result<String, EvidenceError> {
    value
        .get(field)
        .and_then(|field| field.as_str())
        .map(str::to_string)
        .ok_or_else(|| EvidenceError::Malformed(format!("missing string field {field:?}")))
}

/// Minimal JSON string escaping for the few host-written fields.
fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if (ch as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
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
    fn model_pins_round_trip_and_produce_explicit_start_arguments() {
        let text = "claude=claude-opus-5-5,codex=gpt-6.1-sol,pi=google/gemini-3.8-flash";
        let pins = parse_models(text).expect("valid pins");
        assert_eq!(pins.file_value(), text);
        assert_eq!(
            parse_models(&format!("{text}\n")).expect("file newline"),
            pins
        );
        assert_eq!(
            agent_start_args("claude", &pins),
            ["--model", "claude-opus-5-5"]
        );
        assert_eq!(
            agent_start_args("codex", &pins),
            [
                "-m",
                "gpt-6.1-sol",
                "-c",
                "check_for_update_on_startup=false"
            ]
        );
        assert_eq!(
            agent_start_args("pi", &pins),
            ["--provider", "google", "--model", "gemini-3.8-flash"]
        );
        assert!(agent_start_args("unknown", &pins).is_empty());
    }

    #[test]
    fn model_pins_reject_missing_duplicate_empty_and_shell_hostile_values() {
        for value in ["", "a b", "x;y", "$(id)", "a\nb"] {
            assert!(parse_models(&format!("claude={value},codex=b,pi=p/m")).is_err());
        }
        for value in [
            "claude=a,codex=b",
            "claude=a,codex=b,pi=p/m,pi=p/m",
            "claude=a,codex=b,pi=/m",
            "claude=a,codex=b,pi=p/",
            "unknown=a,codex=b,pi=p/m",
        ] {
            assert!(parse_models(value).is_err(), "{value}");
        }
    }

    #[test]
    fn explicit_model_overrides_win_over_environment_without_host_defaults() {
        let flags = ModelOverrides {
            codex: Some("flag-model".into()),
            ..Default::default()
        };
        let env = ModelOverrides {
            codex: Some("env-model".into()),
            pi_model: Some("env-pi".into()),
            ..Default::default()
        };
        let result = flags.with_fallback(env);
        assert_eq!(result.codex.as_deref(), Some("flag-model"));
        assert_eq!(result.pi_model.as_deref(), Some("env-pi"));
        assert_eq!(result.claude, None);
        assert_eq!(result.pi_provider, None);
    }

    #[test]
    fn model_overrides_are_opt_in_validated_kit_arguments() {
        assert!(ModelOverrides::default()
            .kit_args()
            .expect("no defaults")
            .is_empty());
        let overrides = ModelOverrides {
            claude: Some("other-claude".into()),
            codex: Some("other-codex".into()),
            pi_provider: Some("other-provider".into()),
            pi_model: Some("other-model".into()),
        };
        let args = overrides.kit_args().expect("valid overrides");
        assert_eq!(
            args,
            [
                "--kit-arg",
                "claude_model=other-claude",
                "--kit-arg",
                "codex_model=other-codex",
                "--kit-arg",
                "pi_provider=other-provider",
                "--kit-arg",
                "gemini_model=other-model"
            ]
        );
        let roles = rotation_for("demo");
        let argv = sbx_create_argv(DEFAULT_KIT, "/work/repo", "demo", &roles, &[], &args);
        assert!(argv.ends_with(&args));
        assert!(ModelOverrides {
            codex: Some("$(id)".into()),
            ..Default::default()
        }
        .kit_args()
        .is_err());
    }

    #[test]
    fn sbx_create_argv_provisions_named_sandbox_with_roles_and_mixins() {
        let roles = parse_roles("orchestrator=gemini,planner=gemini,implementer=codex,qc=claude")
            .expect("valid roles should parse");
        let argv = sbx_create_argv("./kits/herdr-crew/", "/work/repo", "demo", &roles, &[], &[]);
        assert_eq!(
            argv,
            [
                "sbx",
                "create",
                "./kits/herdr-crew/",
                "/work/repo",
                "--name",
                "herdr-task-demo",
                "--skills",
                "off",
                "--kit-arg",
                "roles=orchestrator=gemini,planner=gemini,implementer=codex,qc=claude",
            ]
        );

        let mixins = vec![
            "git+https://github.com/shelajev/yt-transcript-sbx-kit.git".to_string(),
            "docker.io/example/other-mixin:1.0".to_string(),
        ];
        let argv = sbx_create_argv(
            "./kits/herdr-crew/",
            "/work/repo",
            "demo",
            &roles,
            &mixins,
            &[],
        );
        // Asserted as the trailing flag pairs rather than a fixed index, so
        // inserting another option earlier does not silently need a hand-edited
        // offset here.
        assert_eq!(
            argv[argv.len() - 4..],
            [
                "--kit",
                "git+https://github.com/shelajev/yt-transcript-sbx-kit.git",
                "--kit",
                "docker.io/example/other-mixin:1.0",
            ]
        );
        // sbx_create_argv prefixes the assignment with `roles=`; assert the
        // value actually passed, not the bare assignment.
        assert_eq!(
            flag_value(&argv, "--kit-arg"),
            Some(format!("roles={}", roles.kit_arg()))
        );
    }

    /// The value following `flag` in an argv, so tests assert the pair rather
    /// than a position.
    fn flag_value(argv: &[String], flag: &str) -> Option<String> {
        argv.iter()
            .position(|arg| arg == flag)
            .and_then(|index| argv.get(index + 1))
            .cloned()
    }

    #[test]
    fn task_sandboxes_are_always_created_with_the_shared_skill_store_off() {
        // No host skill store is mounted into a task sandbox, whatever else the
        // caller asks for. Asserted as a flag/value pair so a reordering of
        // sbx_create_argv cannot quietly drop it.
        let roles = rotation_for("anything");
        for mixins in [vec![], vec!["docker.io/example/mixin:1".to_string()]] {
            let argv = sbx_create_argv(DEFAULT_KIT, "/work/repo", "demo", &roles, &mixins, &[]);
            assert_eq!(
                flag_value(&argv, "--skills").as_deref(),
                Some("off"),
                "every task sandbox must be created with --skills off"
            );
        }
    }

    #[test]
    fn the_default_kit_is_a_published_image_not_a_local_path() {
        // The driver must never implicitly build or publish a kit: a task is
        // created from an already-published reference.
        assert!(
            DEFAULT_KIT.starts_with("docker.io/"),
            "the default kit must be a published image reference, got {DEFAULT_KIT}"
        );
        let roles = rotation_for("demo");
        let argv = sbx_create_argv(DEFAULT_KIT, "/work/repo", "demo", &roles, &[], &[]);
        assert_eq!(argv[..3], ["sbx", "create", DEFAULT_KIT]);
    }

    #[test]
    fn codex_updater_recovery_rejects_history_and_unrelated_dialogs() {
        let menu = "Update available!\n› 1. Update now\n  2. Skip\nPress enter to continue";
        assert!(codex_startup_update_menu("codex", "blocked", menu));
        assert!(!codex_startup_update_menu("codex", "working", menu));
        assert!(!codex_startup_update_menu("claude", "blocked", menu));
        assert!(!codex_startup_update_menu(
            "codex",
            "blocked",
            "Do you trust this folder?\nPress enter to continue"
        ));
        assert!(!codex_startup_update_menu(
            "codex",
            "blocked",
            &menu.replace("› 1.", "  1.")
        ));
    }

    // -----------------------------------------------------------------
    // Completion evidence
    // -----------------------------------------------------------------

    const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";

    /// A QC report that should be accepted, so each rejection test can change
    /// exactly one thing and attribute the refusal to that change.
    fn crew() -> RoleAssignment {
        parse_roles("orchestrator=codex,planner=codex,implementer=claude,qc=pi")
            .expect("fixture roles")
    }

    /// Checks a crew might reasonably run for *this* repository. The gate does
    /// not require these names; the fixture only needs some real ones.
    const SAMPLE_CHECKS: [(&str, &str); 2] =
        [("lint", "just lint"), ("tests", "just ci-tests 'all()'")];

    fn good_qc(run_id: &str, goal_digest: &str, workspace: &str, commit: &str) -> String {
        let checks = SAMPLE_CHECKS
            .iter()
            .map(|(name, command)| {
                format!(
                    r#"{{"name":"{name}","command":"{command}","outcome":"passed","exit_code":0}}"#
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let roles = crew();
        format!(
            r#"{{"version":1,"run_id":"{run_id}","goal_digest":"{goal_digest}",
               "workspace":"{workspace}","commit":"{commit}","verdict":"PASS",
               "implementer":"{}","qc":"{}","checks":[{checks}]}}"#,
            roles.implementer, roles.qc
        )
    }

    #[test]
    fn exact_current_evidence_is_accepted() {
        let run = new_run_metadata("ship it", "/home/agent/workspace");
        let accepted = accept_run(
            &crew(),
            Some(&run.to_json()),
            Some(&good_qc(
                &run.run_id,
                &run.goal_digest,
                &run.workspace,
                HEAD,
            )),
            HEAD,
            "",
        )
        .expect("exact current evidence should be accepted");
        assert_eq!(accepted.commit, HEAD);
        assert_eq!(accepted.verdict, "PASS");
    }

    #[test]
    fn a_run_id_is_never_reused_for_the_same_goal() {
        let first = new_run_metadata("ship it", "/home/agent/workspace");
        let second = new_run_metadata("ship it", "/home/agent/workspace");
        assert_ne!(first.run_id, second.run_id);
        assert_eq!(first.goal_digest, second.goal_digest);
    }

    #[test]
    fn missing_artifacts_are_refused_rather_than_treated_as_success() {
        // An older kit writes neither file. That must be an explicit refusal
        // naming the missing artifact, never an acceptance and never a hang.
        let run = new_run_metadata("ship it", "/home/agent/workspace");
        assert_eq!(
            accept_run(&crew(), None, None, HEAD, ""),
            Err(EvidenceError::Missing(RUN_METADATA_FILE))
        );
        assert_eq!(
            accept_run(&crew(), Some(&run.to_json()), None, HEAD, ""),
            Err(EvidenceError::Missing(QC_EVIDENCE_FILE))
        );
        let rendered = EvidenceError::Missing(QC_EVIDENCE_FILE).to_string();
        assert!(rendered.contains("RESULT: DONE` alone is not acceptance"));
    }

    #[test]
    fn evidence_from_a_previous_run_is_stale() {
        let previous = new_run_metadata("ship it", "/home/agent/workspace");
        let current = new_run_metadata("ship it", "/home/agent/workspace");
        let error = accept_run(
            &crew(),
            Some(&current.to_json()),
            Some(&good_qc(
                &previous.run_id,
                &current.goal_digest,
                &current.workspace,
                HEAD,
            )),
            HEAD,
            "",
        )
        .unwrap_err();
        assert!(matches!(error, EvidenceError::Stale(_)), "{error}");
    }

    #[test]
    fn evidence_for_another_goal_or_workspace_is_stale() {
        let run = new_run_metadata("ship it", "/home/agent/workspace");
        let other_goal = accept_run(
            &crew(),
            Some(&run.to_json()),
            Some(&good_qc(
                &run.run_id,
                &digest("something else"),
                &run.workspace,
                HEAD,
            )),
            HEAD,
            "",
        )
        .unwrap_err();
        assert!(
            matches!(other_goal, EvidenceError::Stale(_)),
            "{other_goal}"
        );

        let other_workspace = accept_run(
            &crew(),
            Some(&run.to_json()),
            Some(&good_qc(
                &run.run_id,
                &run.goal_digest,
                "/tmp/elsewhere",
                HEAD,
            )),
            HEAD,
            "",
        )
        .unwrap_err();
        assert!(
            matches!(other_workspace, EvidenceError::Stale(_)),
            "{other_workspace}"
        );
    }

    #[test]
    fn a_commit_after_review_invalidates_the_report() {
        let run = new_run_metadata("ship it", "/home/agent/workspace");
        let reviewed = HEAD;
        let moved_on = "89abcdef0123456789abcdef0123456789abcdef";
        let error = accept_run(
            &crew(),
            Some(&run.to_json()),
            Some(&good_qc(
                &run.run_id,
                &run.goal_digest,
                &run.workspace,
                reviewed,
            )),
            moved_on,
            "",
        )
        .unwrap_err();
        assert_eq!(
            error,
            EvidenceError::CommitMismatch {
                reported: reviewed.to_string(),
                actual: moved_on.to_string(),
            }
        );
    }

    #[test]
    fn an_edit_after_review_invalidates_the_report() {
        let run = new_run_metadata("ship it", "/home/agent/workspace");
        for porcelain in [
            " M src/tasks.rs",
            "M  src/tasks.rs",
            "?? note.txt\n M justfile",
        ] {
            let error = accept_run(
                &crew(),
                Some(&run.to_json()),
                Some(&good_qc(
                    &run.run_id,
                    &run.goal_digest,
                    &run.workspace,
                    HEAD,
                )),
                HEAD,
                porcelain,
            )
            .unwrap_err();
            assert!(
                matches!(error, EvidenceError::DirtyWorktree(_)),
                "{porcelain:?} should invalidate the report, got {error}"
            );
        }
    }

    #[test]
    fn abbreviated_or_malformed_commit_ids_are_refused() {
        let run = new_run_metadata("ship it", "/home/agent/workspace");
        // An abbreviation can match more than one commit, so it cannot pin a review.
        let abbreviated = accept_run(
            &crew(),
            Some(&run.to_json()),
            Some(&good_qc(
                &run.run_id,
                &run.goal_digest,
                &run.workspace,
                &HEAD[..12],
            )),
            HEAD,
            "",
        )
        .unwrap_err();
        assert!(
            matches!(abbreviated, EvidenceError::Malformed(_)),
            "{abbreviated}"
        );

        // Uppercase hex is not what `git rev-parse HEAD` emits; refuse it rather
        // than normalizing, so the report has to carry the real value.
        let uppercase = HEAD.to_ascii_uppercase();
        assert!(!is_full_commit_id(&uppercase));

        // An unreadable HEAD (ssh failure, empty output) must fail closed.
        let unreadable = accept_run(
            &crew(),
            Some(&run.to_json()),
            Some(&good_qc(
                &run.run_id,
                &run.goal_digest,
                &run.workspace,
                HEAD,
            )),
            "",
            "",
        )
        .unwrap_err();
        assert!(
            matches!(unreadable, EvidenceError::Malformed(_)),
            "{unreadable}"
        );
    }

    #[test]
    fn unsupported_schema_versions_cannot_pass() {
        let run = new_run_metadata("ship it", "/home/agent/workspace");
        let future = good_qc(&run.run_id, &run.goal_digest, &run.workspace, HEAD)
            .replace("\"version\":1", "\"version\":2");
        assert_eq!(
            accept_run(&crew(), Some(&run.to_json()), Some(&future), HEAD, ""),
            Err(EvidenceError::UnsupportedVersion(2))
        );
    }

    #[test]
    fn malformed_reports_cannot_pass() {
        let run = new_run_metadata("ship it", "/home/agent/workspace");
        let good = good_qc(&run.run_id, &run.goal_digest, &run.workspace, HEAD);
        for broken in [
            "not json at all".to_string(),
            "{}".to_string(),
            good.replace("\"verdict\":\"PASS\"", "\"verdict\":null"),
            good.replace("\"checks\":[", "\"checks\":\"ran them\"["),
            good.replace("\"commit\"", "\"commit_id\""),
        ] {
            let error =
                accept_run(&crew(), Some(&run.to_json()), Some(&broken), HEAD, "").unwrap_err();
            assert!(
                matches!(
                    error,
                    EvidenceError::Malformed(_) | EvidenceError::UnsupportedVersion(_)
                ),
                "{broken:?} should not parse, got {error}"
            );
        }
    }

    #[test]
    fn a_fail_or_blocked_verdict_is_not_success() {
        let run = new_run_metadata("ship it", "/home/agent/workspace");
        for verdict in ["FAIL", "BLOCKED", "pass", ""] {
            let report = good_qc(&run.run_id, &run.goal_digest, &run.workspace, HEAD).replace(
                "\"verdict\":\"PASS\"",
                &format!("\"verdict\":\"{verdict}\""),
            );
            let error =
                accept_run(&crew(), Some(&run.to_json()), Some(&report), HEAD, "").unwrap_err();
            assert!(
                matches!(error, EvidenceError::NotAccepted(_)),
                "verdict {verdict:?} should not be accepted, got {error}"
            );
        }
    }

    #[test]
    fn a_pass_with_no_checks_or_a_failing_check_is_not_evidence() {
        let run = new_run_metadata("ship it", "/home/agent/workspace");
        let base = good_qc(&run.run_id, &run.goal_digest, &run.workspace, HEAD);

        // A PASS that ran nothing is a claim, not evidence.
        let checks_at = base.find("\"checks\":[").expect("fixture has checks");
        let empty = format!("{}\"checks\":[]}}", &base[..checks_at]);
        let error = accept_run(&crew(), Some(&run.to_json()), Some(&empty), HEAD, "").unwrap_err();
        assert!(matches!(error, EvidenceError::NotAccepted(_)), "{error}");

        // A PASS may not carry a check that failed, was blocked, or was skipped,
        // whatever that check happens to be.
        for (name, command) in SAMPLE_CHECKS {
            let good = format!(
                r#"{{"name":"{name}","command":"{command}","outcome":"passed","exit_code":0}}"#
            );
            for bad in [
                format!(
                    r#"{{"name":"{name}","command":"{command}","outcome":"failed","exit_code":1}}"#
                ),
                format!(
                    r#"{{"name":"{name}","command":"{command}","outcome":"blocked","exit_code":0}}"#
                ),
                format!(
                    r#"{{"name":"{name}","command":"{command}","outcome":"skipped","exit_code":0}}"#
                ),
                // "passed" with a nonzero status contradicts itself.
                format!(
                    r#"{{"name":"{name}","command":"{command}","outcome":"passed","exit_code":1}}"#
                ),
                // "passed" with no status at all did not record an outcome.
                format!(r#"{{"name":"{name}","command":"{command}","outcome":"passed"}}"#),
            ] {
                let mutated = base.replace(&good, &bad);
                assert_ne!(mutated, base, "fixture mutation for {name} did not apply");
                let error = accept_run(&crew(), Some(&run.to_json()), Some(&mutated), HEAD, "")
                    .unwrap_err();
                assert!(
                    matches!(error, EvidenceError::NotAccepted(_)),
                    "a PASS carrying {bad} must be refused, got {error}"
                );
            }
        }
    }

    #[test]
    fn a_check_that_does_not_say_what_ran_is_not_evidence() {
        // A green row that names nothing is indistinguishable from no check at
        // all, so "passed, exit 0" must not carry it past the gate.
        let run = new_run_metadata("ship it", "/home/agent/workspace");
        let base = good_qc(&run.run_id, &run.goal_digest, &run.workspace, HEAD);
        let (name, command) = SAMPLE_CHECKS[0];
        let good = format!(
            r#"{{"name":"{name}","command":"{command}","outcome":"passed","exit_code":0}}"#
        );

        for (blank_name, blank_command) in [
            ("", command),
            ("   ", command),
            ("\\t", command),
            ("\\n", command),
            (name, ""),
            (name, "   "),
            (name, "\\t\\n"),
            ("", ""),
        ] {
            let bad = format!(
                r#"{{"name":"{blank_name}","command":"{blank_command}","outcome":"passed","exit_code":0}}"#
            );
            let mutated = base.replace(&good, &bad);
            assert_ne!(mutated, base, "fixture mutation did not apply");
            let error =
                accept_run(&crew(), Some(&run.to_json()), Some(&mutated), HEAD, "").unwrap_err();
            assert!(
                matches!(error, EvidenceError::NotAccepted(_)),
                "name {blank_name:?} / command {blank_command:?} must be refused, got {error}"
            );
            assert!(error.to_string().contains("must name what ran"));
        }
    }

    #[test]
    fn the_report_must_come_from_the_configured_qc_role() {
        let run = new_run_metadata("ship it", "/home/agent/workspace");
        let roles = crew();
        let base = good_qc(&run.run_id, &run.goal_digest, &run.workspace, HEAD);

        // The implementer signing off under the QC field.
        let self_review = base.replace(
            &format!(r#""qc":"{}""#, roles.qc),
            &format!(r#""qc":"{}""#, roles.implementer),
        );
        let error =
            accept_run(&roles, Some(&run.to_json()), Some(&self_review), HEAD, "").unwrap_err();
        assert!(matches!(error, EvidenceError::NotAccepted(_)), "{error}");

        // A reviewer this task was never configured with.
        let invented = base.replace(
            &format!(r#""qc":"{}""#, roles.qc),
            r#""qc":"some-other-model""#,
        );
        let error =
            accept_run(&roles, Some(&run.to_json()), Some(&invented), HEAD, "").unwrap_err();
        assert!(matches!(error, EvidenceError::NotAccepted(_)), "{error}");
        assert!(error.to_string().contains("configured with"));

        // An implementer this task was never configured with.
        let wrong_implementer = base.replace(
            &format!(r#""implementer":"{}""#, roles.implementer),
            r#""implementer":"someone-else""#,
        );
        let error = accept_run(
            &roles,
            Some(&run.to_json()),
            Some(&wrong_implementer),
            HEAD,
            "",
        )
        .unwrap_err();
        assert!(matches!(error, EvidenceError::NotAccepted(_)), "{error}");
    }

    #[test]
    fn the_gate_does_not_require_any_particular_check_name() {
        // The driver runs arbitrary repositories and non-coding goals, so a
        // crew that ran checks appropriate to *its* task must be acceptable.
        let run = new_run_metadata("update the handbook", "/home/agent/workspace");
        let roles = crew();
        let report = format!(
            r#"{{"version":1,"run_id":"{}","goal_digest":"{}","workspace":"{}",
               "commit":"{HEAD}","verdict":"PASS","implementer":"{}","qc":"{}",
               "checks":[{{"name":"link-check","command":"lychee docs/","outcome":"passed","exit_code":0}},
                         {{"name":"spelling","command":"typos","outcome":"passed","exit_code":0}}]}}"#,
            run.run_id, run.goal_digest, run.workspace, roles.implementer, roles.qc
        );
        let accepted = accept_run(&roles, Some(&run.to_json()), Some(&report), HEAD, "")
            .expect("task-appropriate checks should be accepted");
        assert_eq!(accepted.checks.len(), 2);
    }

    #[test]
    fn run_metadata_round_trips_through_json() {
        let run = new_run_metadata(
            "a goal with \"quotes\" and\na newline",
            "/home/agent/workspace",
        );
        assert_eq!(RunMetadata::parse(&run.to_json()).unwrap(), run);
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
