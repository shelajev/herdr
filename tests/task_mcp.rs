#![cfg(unix)]
//! `herdr task new --mcp NAME` through the real binary, with fake `sbx` and `ssh`
//! executables on PATH (see `tests/fixtures/fake-sbx/`). The fakes are not Docker
//! SBX: they record the argv herdr builds and resolve `--static-mcp` names against
//! a host-side registry the way a gateway would, so these tests prove what herdr
//! hands to `sbx`, that bad names stop before `sbx` runs, and that `task rm` leaves
//! host MCP registrations alone. They do not prove real SBX behavior.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const FAKE_SBX: &str = include_str!("fixtures/fake-sbx/sbx");
const FAKE_SSH: &str = include_str!("fixtures/fake-sbx/ssh");

/// A scratch directory under the system temp dir, removed on drop.
struct Fixture {
    root: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

impl Fixture {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "herdr-task-mcp-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("fixture root");
        let bin = root.join("fakebin");
        fs::create_dir_all(&bin).expect("fakebin");
        for (name, body) in [("sbx", FAKE_SBX), ("ssh", FAKE_SSH)] {
            let path = bin.join(name);
            fs::write(&path, body).expect("write fake");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod fake");
        }
        fs::create_dir_all(root.join("state")).expect("state");
        fs::create_dir_all(root.join("workspace")).expect("workspace");
        Self { root }
    }

    fn state(&self) -> PathBuf {
        self.root.join("state")
    }

    fn workspace(&self) -> PathBuf {
        self.root.join("workspace")
    }

    fn command(&self, program: &str) -> Command {
        let mut command = if program == "herdr" {
            Command::new(env!("CARGO_BIN_EXE_herdr"))
        } else {
            Command::new(self.root.join("fakebin").join(program))
        };
        let path = format!(
            "{}:{}",
            self.root.join("fakebin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        command
            .env("PATH", path)
            .env("FAKE_SBX_STATE", self.state())
            .env_remove("HERDR_TASK_MCP")
            .env_remove("HERDR_TASK_KIT")
            .env_remove("HERDR_TASK_CLAUDE_MODEL")
            .env_remove("HERDR_TASK_CODEX_MODEL")
            .env_remove("HERDR_TASK_PI_PROVIDER")
            .env_remove("HERDR_TASK_PI_MODEL");
        command
    }

    /// Register a host MCP server with the fake registry, as the host would with
    /// `sbx mcp add`.
    fn register(&self, name: &str, command: &str, args: &[&str]) {
        let mut cmd = self.command("sbx");
        cmd.args(["mcp", "add", name, "--command", command]);
        for arg in args {
            cmd.arg(format!("--args={arg}"));
        }
        assert!(cmd.output().expect("run fake sbx").status.success());
    }

    fn task_new(&self, slug: &str, extra: &[&str]) -> Output {
        self.task_new_with_env(slug, extra, &[])
    }

    fn task_new_with_env(&self, slug: &str, extra: &[&str], env: &[(&str, &str)]) -> Output {
        let mut command = self.command("herdr");
        command
            .args(["task", "new", slug, "--dir"])
            .arg(self.workspace())
            .args(extra);
        for (key, value) in env {
            command.env(key, value);
        }
        command.output().expect("run herdr")
    }

    /// The recorded calls as argv vectors.
    fn calls(&self) -> Vec<Vec<String>> {
        let log = fs::read_to_string(self.state().join("calls.log")).unwrap_or_default();
        let mut calls: Vec<Vec<String>> = Vec::new();
        for line in log.lines() {
            if line == "--- call" {
                calls.push(Vec::new());
            } else if let Some(call) = calls.last_mut() {
                call.push(line.to_string());
            }
        }
        calls
    }

    /// The `sbx create` calls only (registration calls from the test setup are
    /// `sbx mcp add`).
    fn creates(&self) -> Vec<Vec<String>> {
        self.calls()
            .into_iter()
            .filter(|call| call.first().map(String::as_str) == Some("create"))
            .collect()
    }

    fn launcher(&self, slug: &str, mcp: &str) -> PathBuf {
        self.state()
            .join("sandboxes")
            .join(format!("herdr-task-{slug}"))
            .join(format!("mcp-{mcp}.sh"))
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn static_mcp_names(create: &[String]) -> Vec<&str> {
    create
        .windows(2)
        .filter(|pair| pair[0] == "--static-mcp")
        .map(|pair| pair[1].as_str())
        .collect()
}

fn flag_value<'a>(argv: &'a [String], flag: &str) -> Option<&'a str> {
    argv.iter()
        .position(|arg| arg == flag)
        .and_then(|index| argv.get(index + 1))
        .map(String::as_str)
}

const HOST_BEANS_ARGS: [&str; 6] = [
    "--beans-bin",
    "/host/bin/beans",
    "--beans-config",
    "/host/store/.beans.yml",
    "--beans-data",
    "/host/store/.beans",
];

#[test]
fn mcp_flag_passes_a_registered_static_mcp_to_sbx_create_and_the_host_command_is_what_runs() {
    let fx = Fixture::new();
    fx.register("central-beans", "/host/bin/beans-mcp", &HOST_BEANS_ARGS);

    let output = fx.task_new(
        "demo",
        &[
            "--mixin",
            "git+https://example.invalid/acr.git#ref=abc",
            "--mcp",
            "central-beans",
        ],
    );
    assert!(
        output.status.success(),
        "task new failed: {}",
        stderr(&output)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("mcp:       central-beans"), "{stdout}");
    assert!(stdout.contains("task demo is ready."), "{stdout}");

    let creates = fx.creates();
    assert_eq!(creates.len(), 1, "exactly one sbx create: {creates:?}");
    let create = &creates[0];
    assert_eq!(flag_value(create, "--skills"), Some("off"));
    assert_eq!(flag_value(create, "--name"), Some("herdr-task-demo"));
    assert_eq!(static_mcp_names(create), ["central-beans"]);
    // Tail: mixin as --kit, then the --static-mcp pair.
    assert_eq!(
        create[create.len() - 4..],
        [
            "--kit",
            "git+https://example.invalid/acr.git#ref=abc",
            "--static-mcp",
            "central-beans"
        ]
    );
    // No sandbox-side input reaches the host command: the launcher is exactly the
    // registered command and arguments, with nothing from the workspace or goal.
    let launcher = fs::read_to_string(fx.launcher("demo", "central-beans")).expect("launcher");
    assert_eq!(
        launcher.trim_end(),
        "#!/bin/sh\nexec '/host/bin/beans-mcp' '--beans-bin' '/host/bin/beans' '--beans-config' '/host/store/.beans.yml' '--beans-data' '/host/store/.beans'"
    );
    // herdr never calls `sbx mcp` itself: registration is the host's.
    assert!(
        fx.calls()
            .iter()
            .filter(|call| call[0] == "mcp")
            .all(|call| call[1] == "add" && call[2] == "central-beans"),
        "herdr must not register or change MCP servers: {:?}",
        fx.calls()
    );
}

#[test]
fn repeated_mcp_flags_pass_one_static_mcp_pair_per_name_in_order() {
    let fx = Fixture::new();
    fx.register("central-beans", "/host/bin/beans-mcp", &[]);
    fx.register("second_server", "/host/bin/other", &[]);

    let output = fx.task_new(
        "demo",
        &["--mcp", "central-beans", "--mcp", "second_server"],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let creates = fx.creates();
    assert_eq!(
        static_mcp_names(&creates[0]),
        ["central-beans", "second_server"]
    );
    assert!(fx.launcher("demo", "central-beans").exists());
    assert!(fx.launcher("demo", "second_server").exists());
}

#[test]
fn host_environment_default_applies_only_without_flags() {
    let fx = Fixture::new();
    fx.register("central-beans", "/host/bin/beans-mcp", &[]);
    fx.register("flagged", "/host/bin/flagged", &[]);

    let output = fx.task_new_with_env("env-default", &[], &[("HERDR_TASK_MCP", "central-beans")]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(static_mcp_names(&fx.creates()[0]), ["central-beans"]);

    let output = fx.task_new_with_env(
        "flag-wins",
        &["--mcp", "flagged"],
        &[("HERDR_TASK_MCP", "central-beans")],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(static_mcp_names(&fx.creates()[1]), ["flagged"]);
}

#[test]
fn without_mcp_the_create_argv_has_no_static_mcp_and_keeps_its_legacy_shape() {
    let fx = Fixture::new();
    let output = fx.task_new("legacy", &["--mixin", "docker.io/example/mixin:1"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let creates = fx.creates();
    assert_eq!(creates.len(), 1);
    let create = &creates[0];
    assert!(static_mcp_names(create).is_empty());
    assert_eq!(flag_value(create, "--skills"), Some("off"));
    assert_eq!(
        create[create.len() - 2..],
        ["--kit", "docker.io/example/mixin:1"]
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("mcp:"));
}

#[test]
fn invalid_mcp_names_stop_before_sbx_runs() {
    let fx = Fixture::new();
    let long = "a".repeat(65);
    let bad: Vec<&str> = vec![
        "",
        "../central-beans",
        "/usr/bin/beans-mcp",
        "~/beans-mcp",
        "https://example.com/mcp",
        "central beans",
        "central;id",
        "central$(id)",
        "central`id`",
        "central|cat",
        "central&&id",
        "-central",
        "--kit",
        "Central",
        "caf\u{e9}",
        "central\nother",
        &long,
    ];
    for name in bad {
        let output = fx.task_new("demo", &["--mcp", name]);
        assert_eq!(output.status.code(), Some(2), "{name:?} was not rejected");
        assert!(
            stderr(&output).contains("invalid MCP server"),
            "{name:?}: {}",
            stderr(&output)
        );
    }
    // Duplicates, and a bad name hidden among good ones.
    for extra in [
        vec!["--mcp", "dup", "--mcp", "dup"],
        vec!["--mcp", "good", "--mcp", "bad/name"],
    ] {
        let output = fx.task_new("demo", &extra);
        assert_eq!(output.status.code(), Some(2), "{extra:?}");
    }
    // A missing value and the unsupported `--mcp=NAME` spelling are usage errors.
    assert_eq!(fx.task_new("demo", &["--mcp"]).status.code(), Some(2));
    assert_eq!(
        fx.task_new("demo", &["--mcp=central-beans"]).status.code(),
        Some(2)
    );
    // A bad host default is rejected too, naming the variable.
    let output = fx.task_new_with_env("demo", &[], &[("HERDR_TASK_MCP", "good,../bad")]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("HERDR_TASK_MCP"),
        "{}",
        stderr(&output)
    );

    assert!(
        fx.calls().is_empty(),
        "sbx must not run for rejected input: {:?}",
        fx.calls()
    );
}

#[test]
fn an_unregistered_name_fails_creation_with_a_registration_hint_and_no_readiness_wait() {
    let fx = Fixture::new();
    let output = fx.task_new("demo", &["--mcp", "not-registered"]);
    assert_eq!(output.status.code(), Some(1));
    let err = stderr(&output);
    assert!(err.contains("sbx create failed"), "{err}");
    assert!(err.contains("must already be registered"), "{err}");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("is ready"));
    assert!(!fx.launcher("demo", "not-registered").exists());
}

#[test]
fn task_rm_only_removes_the_sandbox_and_leaves_mcp_registrations_alone() {
    let fx = Fixture::new();
    fx.register("central-beans", "/host/bin/beans-mcp", &[]);
    assert!(fx
        .task_new("demo", &["--mcp", "central-beans"])
        .status
        .success());

    let output = fx
        .command("herdr")
        .args(["task", "rm", "demo"])
        .output()
        .expect("run herdr task rm");
    assert!(output.status.success(), "{}", stderr(&output));

    let calls = fx.calls();
    let rm_calls: Vec<&Vec<String>> = calls.iter().filter(|call| call[0] == "rm").collect();
    assert_eq!(rm_calls.len(), 1);
    assert_eq!(rm_calls[0], &["rm", "herdr-task-demo"]);
    // The only `sbx mcp` call in the whole run is the test's own registration.
    assert_eq!(calls.iter().filter(|call| call[0] == "mcp").count(), 1);
    assert!(Path::new(&fx.state().join("mcp").join("central-beans")).exists());
}
