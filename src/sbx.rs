use crate::detect::Agent;

/// Docker Sandboxes agents whose screen manifests expose positive idle evidence.
///
/// SBX startup can spend a long time preparing a microVM. Restrict managed launches
/// to agents for which Herdr can distinguish that setup output from a ready prompt.
pub(crate) const SUPPORTED_AGENTS: &[Agent] = &[Agent::Claude, Agent::Codex, Agent::Kiro];

pub(crate) fn supports_agent(agent: Agent) -> bool {
    SUPPORTED_AGENTS.contains(&agent)
}

pub(crate) fn launch_argv(
    session_name: Option<&str>,
    agent_name: &str,
    agent: Agent,
    agent_args: &[String],
) -> Option<Vec<String>> {
    supports_agent(agent).then_some(())?;
    let sbx_agent = crate::detect::agent_label(agent);
    let sandbox_name = sandbox_name(session_name, agent_name);
    let mut argv = vec![
        "sbx".to_string(),
        "run".to_string(),
        "--name".to_string(),
        sandbox_name,
        sbx_agent.to_string(),
        ".".to_string(),
    ];
    if !agent_args.is_empty() {
        argv.push("--".to_string());
        argv.extend_from_slice(agent_args);
    }
    Some(argv)
}

pub(crate) fn agent_from_process_argv(
    process_name: &str,
    argv: Option<&[String]>,
) -> Option<Agent> {
    if normalized_executable(process_name) != "sbx" {
        return None;
    }
    let argv = argv?;
    let mut args = argv.iter();
    if args
        .next()
        .is_none_or(|arg| normalized_executable(arg) != "sbx")
    {
        return None;
    }
    if args.next().map(String::as_str) != Some("run") {
        return None;
    }

    while let Some(arg) = args.next() {
        if arg == "--" {
            return None;
        }
        if matches!(
            arg.as_str(),
            "--clone" | "-d" | "--detached" | "-q" | "--quiet"
        ) {
            continue;
        }
        if sbx_option_with_inline_value(arg) {
            continue;
        }
        if sbx_option_takes_value(arg) {
            args.next()?;
            continue;
        }
        if arg.starts_with('-') {
            return None;
        }

        let agent = crate::detect::parse_canonical_agent_label(arg)?;
        return supports_agent(agent).then_some(agent);
    }
    None
}

fn sandbox_name(session_name: Option<&str>, agent_name: &str) -> String {
    let session_name = session_name.unwrap_or(crate::session::DEFAULT_SESSION_NAME);
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in session_name
        .bytes()
        .chain(std::iter::once(0))
        .chain(agent_name.bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let safe_agent_name = agent_name.replace('_', ".");
    format!("herdr-{safe_agent_name}-{:08x}", hash as u32)
}

fn normalized_executable(value: &str) -> String {
    let mut executable = value
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or(value)
        .to_ascii_lowercase();
    if executable.ends_with(".exe") {
        executable.truncate(executable.len() - ".exe".len());
    }
    executable
}

fn sbx_option_takes_value(arg: &str) -> bool {
    matches!(
        arg,
        "--cpus"
            | "--deny-network"
            | "-e"
            | "--env"
            | "--env-file"
            | "--kit"
            | "-m"
            | "--memory"
            | "--name"
            | "-p"
            | "--publish"
            | "-t"
            | "--template"
    )
}

fn sbx_option_with_inline_value(arg: &str) -> bool {
    const OPTIONS: &[&str] = &[
        "--cpus",
        "--deny-network",
        "--env",
        "--env-file",
        "--kit",
        "--memory",
        "--name",
        "--publish",
        "--template",
    ];
    OPTIONS.iter().any(|option| {
        arg.strip_prefix(option)
            .is_some_and(|rest| rest.starts_with('='))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_argv_keeps_sbx_and_agent_arguments_separate() {
        let argv = launch_argv(
            Some("factory_one"),
            "dev_one",
            Agent::Codex,
            &["--model".into(), "gpt 5".into(), "$(touch nope)".into()],
        )
        .unwrap();

        assert_eq!(argv[0..4], ["sbx", "run", "--name", argv[3].as_str()]);
        assert!(argv[3].starts_with("herdr-dev.one-"));
        assert_eq!(
            argv[4..],
            ["codex", ".", "--", "--model", "gpt 5", "$(touch nope)"]
        );
    }

    #[test]
    fn sandbox_names_are_stable_and_session_scoped() {
        let first = sandbox_name(Some("one"), "worker");
        assert_eq!(first, sandbox_name(Some("one"), "worker"));
        assert_ne!(first, sandbox_name(Some("two"), "worker"));
        assert_ne!(sandbox_name(None, "a_b"), sandbox_name(None, "a-b"));
        assert!(first
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-')));
    }

    #[test]
    fn process_detection_recognizes_supported_sbx_run_shapes() {
        for argv in [
            vec!["sbx", "run", "codex", "."],
            vec!["sbx.exe", "run", "--name", "reviewer", "claude", "."],
            vec!["/opt/docker/sbx", "run", "--name=worker", "kiro", "."],
            vec!["sbx", "run", "codex", "--name", "reviewer"],
        ] {
            let argv = argv.into_iter().map(str::to_string).collect::<Vec<_>>();
            assert!(agent_from_process_argv(&argv[0], Some(&argv)).is_some());
        }
    }

    #[test]
    fn process_detection_rejects_other_sbx_commands_and_unsupported_agents() {
        for argv in [
            vec!["sbx", "exec", "worker", "codex"],
            vec!["sbx", "run", "opencode", "."],
            vec!["sbx", "run", "--unknown", "codex", "."],
            vec!["not-sbx", "run", "codex", "."],
        ] {
            let argv = argv.into_iter().map(str::to_string).collect::<Vec<_>>();
            assert_eq!(agent_from_process_argv(&argv[0], Some(&argv)), None);
        }
    }
}
