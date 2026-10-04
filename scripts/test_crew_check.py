"""Exercise crew-check against stub tools and the kit's Codex trust seeding step."""
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import tomllib
import unittest

ROOT = Path(__file__).resolve().parents[1]
CREW_CHECK = ROOT / "kits/herdr-crew/template/crew-check"
DOCKERFILE = ROOT / "kits/herdr-crew/template/Dockerfile"
SPEC = ROOT / "kits/herdr-crew/spec.yaml"
TOOLS = ["node", "herdr", "claude", "codex", "pi", "beans"]
VERSIONS = {
    "node": "v22.22.1",
    "herdr": "herdr 0.9.3",
    "claude": "2.1.289 (Claude Code)",
    "codex": "codex-cli 0.153.4",
    "pi": "0.85.1",
    "beans": "beans 0.4.2",
}
# crew-check needs only these external commands besides bash builtins.
SYSTEM_COMMANDS = ["cat", "mktemp", "rm"]


def stub(path, stdout="", status=0, stderr=""):
    path.write_text(
        "#!/bin/sh\n"
        f"printf '%s' '{stderr}' >&2\n"
        f"printf '%s' '{stdout}'\n"
        f"exit {status}\n"
    )
    path.chmod(0o755)


class CrewCheckTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="crew-check-test-")
        self.addCleanup(self.temp.cleanup)
        root = Path(self.temp.name)
        # A sealed PATH: stub tools plus the few system commands crew-check
        # uses, so a real claude/codex/pi on the developer's machine can never
        # satisfy or mask a case.
        self.bin = root / "bin"
        self.bin.mkdir()
        for command in SYSTEM_COMMANDS:
            (self.bin / command).symlink_to(shutil.which(command))
        for tool, version in VERSIONS.items():
            stub(self.bin / tool, stdout=version + "\n")
        self.empty = root / "empty"
        self.empty.mkdir()

    def run_check(self):
        env = {"PATH": str(self.bin), "CREW_CHECK_BIN_DIR": str(self.empty)}
        return subprocess.run(
            [shutil.which("bash"), str(CREW_CHECK)],
            env=env, capture_output=True, text=True, timeout=30,
        )

    def assertFails(self, result, *fragments):
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertNotIn("tools present", result.stdout)
        for fragment in fragments:
            self.assertIn(fragment, result.stderr)

    def test_success_reports_every_version(self):
        result = self.run_check()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stderr, "")
        for tool, version in VERSIONS.items():
            self.assertRegex(result.stdout, rf"(?m)^  {tool} +{re.escape(version)}$")

    def test_stderr_noise_on_success_never_reaches_the_report(self):
        stub(self.bin / "codex", stdout="codex-cli 0.153.4\n", stderr="WARNING: noise")
        result = self.run_check()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("noise", result.stdout)

    def test_missing_tool_fails_naming_it(self):
        for tool in TOOLS:
            with self.subTest(tool=tool):
                saved = (self.bin / tool).read_text()
                (self.bin / tool).unlink()
                try:
                    result = self.run_check()
                finally:
                    (self.bin / tool).write_text(saved)
                    (self.bin / tool).chmod(0o755)
                self.assertFails(result, f"missing tools: {tool}", "build-crew-template.sh")

    def test_failing_version_command_fails_with_status_and_stderr(self):
        for tool in TOOLS:
            with self.subTest(tool=tool):
                stub(self.bin / tool, stdout="looks fine\n", status=7, stderr="boom from " + tool)
                result = self.run_check()
                self.assertFails(result, f"{tool}:", "exited with status 7", "boom from " + tool)
                stub(self.bin / tool, stdout=VERSIONS[tool] + "\n")

    def test_empty_or_blank_version_output_fails(self):
        for tool in TOOLS:
            for blank in ("", "\n", "  \n\n"):
                with self.subTest(tool=tool, blank=blank):
                    stub(self.bin / tool, stdout=blank, stderr="only stderr")
                    result = self.run_check()
                    self.assertFails(result, f"{tool}:", "reported an empty version")
                    stub(self.bin / tool, stdout=VERSIONS[tool] + "\n")

    def test_node_below_each_floor_fails(self):
        cases = {
            # (version, consumers that must be named as violated)
            "v20.19.4": ["claude", "pi"],
            "v21.7.3": ["claude", "pi"],
            "v22.18.9": ["pi"],
            "v22.0.0": ["pi"],
            "v16.0.0": ["claude", "pi"],
            "v15.14.0": ["claude", "pi", "codex"],
        }
        for version, consumers in cases.items():
            with self.subTest(version=version):
                stub(self.bin / "node", stdout=version + "\n")
                result = self.run_check()
                self.assertFails(result, f"node {version}")
                for consumer in ("claude", "pi", "codex"):
                    marker = f"below the {consumer} floor"
                    if consumer in consumers:
                        self.assertIn(marker, result.stderr)
                    else:
                        self.assertNotIn(marker, result.stderr)

    def test_node_exactly_at_or_above_floors_passes(self):
        for version in ("v22.19.0", "v22.22.1", "v22.23.3", "v23.0.0", "v24.1.0"):
            with self.subTest(version=version):
                stub(self.bin / "node", stdout=version + "\n")
                result = self.run_check()
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn(version, result.stdout)

    def test_unparseable_node_version_fails(self):
        stub(self.bin / "node", stdout="not-a-version\n")
        self.assertFails(self.run_check(), "cannot parse version 'not-a-version'")

    def test_all_problems_are_reported_together(self):
        stub(self.bin / "node", stdout="v20.19.4\n")
        stub(self.bin / "pi", status=3)
        stub(self.bin / "beans", stdout="")
        result = self.run_check()
        self.assertFails(
            result,
            "below the claude floor",
            "pi: 'pi --version' exited with status 3",
            "beans: 'beans version' reported an empty version",
            "4 check(s) failed",
        )

    def test_dockerfile_pin_satisfies_every_floor_and_is_checksummed(self):
        dockerfile = DOCKERFILE.read_text()
        version = re.search(r"(?m)^ARG NODE_VERSION=(\d+)\.(\d+)\.(\d+)$", dockerfile)
        self.assertIsNotNone(version, "Dockerfile must pin an exact NODE_VERSION")
        pinned = tuple(int(part) for part in version.groups())
        floors = re.findall(r'(?m)^  "(\w+) (\d+) (\d+) (\d+)"$', CREW_CHECK.read_text())
        self.assertEqual({name for name, *_ in floors}, {"claude", "pi", "codex"})
        for name, *numbers in floors:
            self.assertGreaterEqual(pinned, tuple(int(n) for n in numbers), name)
        for arch in ("X64", "ARM64"):
            self.assertRegex(dockerfile, rf"(?m)^ARG NODE_SHA256_{arch}=[0-9a-f]{{64}}$")
        self.assertIn("sha256sum -c", dockerfile)
        self.assertIn("https://nodejs.org/dist/v${NODE_VERSION}/", dockerfile)


def install_step(description):
    """Return (index, command, user) of the spec install entry with this description.

    The spec has no YAML parser available to the Python tests, so read the
    block scalar the way YAML does: the lines after `- command: |` with their
    common indentation removed.
    """
    lines = SPEC.read_text().splitlines()
    setup = lines.index("setup:")
    index = -1
    for number in range(setup, len(lines)):
        stripped = lines[number].strip()
        if stripped.startswith("- command:"):
            index += 1
            start = number
        if stripped == f"description: {description}":
            break
    else:
        raise AssertionError(f"no install step described {description!r}")
    assert lines[start].strip() == "- command: |", lines[start]
    body = []
    for line in lines[start + 1:number]:
        if re.match(r"^      user: ", line):
            user = line.split(":", 1)[1].strip().strip('"')
            break
        body.append(line)
    indent = min(len(line) - len(line.lstrip()) for line in body if line.strip())
    return index, "\n".join(line[indent:] for line in body) + "\n", user


class CodexTrustSeedingTests(unittest.TestCase):
    CONFIG = 'approval_policy = "never"\ncheck_for_update_on_startup = false\n'

    @classmethod
    def setUpClass(cls):
        cls.index, command, cls.user = install_step("Seed Codex trust for the task workspace only")
        cls.command = command

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="crew-trust-test-")
        self.addCleanup(self.temp.cleanup)
        self.codex = Path(self.temp.name) / "codex-home"
        self.codex.mkdir()
        self.config = self.codex / "config.toml"
        self.config.write_text(self.CONFIG)

    def seed(self, workspace, shell="/bin/sh"):
        env = {"PATH": os.environ["PATH"]}
        if workspace is not None:
            env["WORKSPACE_DIR"] = workspace
        script = self.command.replace("/home/agent/.codex", str(self.codex))
        return subprocess.run(
            [shell, "-c", script], env=env, capture_output=True, text=True, timeout=30
        )

    def projects(self):
        return tomllib.loads(self.config.read_text()).get("projects", {})

    def shells(self):
        return [shell for shell in ("/bin/sh", shutil.which("bash")) if shell]

    def test_runs_as_agent_after_the_config_step_that_rewrites_the_file(self):
        self.assertEqual(self.user, "agent")
        config_index, _, _ = install_step("Seed Codex config/auth from SBX_CRED_OPENAI_MODE")
        self.assertGreater(self.index, config_index)

    def test_trusts_exactly_the_workspace(self):
        for shell in self.shells():
            with self.subTest(shell=shell):
                self.config.write_text(self.CONFIG)
                result = self.seed("/Users/dev/proj", shell)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(
                    self.config.read_text(),
                    self.CONFIG + '[projects."/Users/dev/proj"]\ntrust_level = "trusted"\n',
                )
                self.assertEqual(
                    self.projects(), {"/Users/dev/proj": {"trust_level": "trusted"}}
                )

    def test_trailing_slashes_are_normalized(self):
        self.assertEqual(self.seed("/work/space//").returncode, 0)
        self.assertEqual(list(self.projects()), ["/work/space"])

    def test_root_empty_unset_and_relative_workspaces_are_refused(self):
        for workspace in ("/", "//", "", None, "relative/dir", "."):
            with self.subTest(workspace=workspace):
                result = self.seed(workspace)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("not seeding Codex trust", result.stderr)
                self.assertEqual(self.config.read_text(), self.CONFIG)
                self.assertEqual(self.projects(), {})

    def test_control_characters_are_refused(self):
        for workspace in ("/work/a\nb", "/work/a\tb", "/work/a\rb"):
            with self.subTest(workspace=repr(workspace)):
                result = self.seed(workspace)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("control characters", result.stderr)
                self.assertEqual(self.config.read_text(), self.CONFIG)

    def test_quotes_backslashes_and_toml_syntax_in_the_path_are_escaped(self):
        paths = [
            '/work/we"ird\\dir',
            "/work/trailing\\",
            '/work/"][projects."/"]trust_level',
            "/work/with space/[brackets]/it's",
            "/work/ünïcode",
        ]
        for path in paths:
            for shell in self.shells():
                with self.subTest(path=path, shell=shell):
                    self.config.write_text(self.CONFIG)
                    result = self.seed(path, shell)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    # The file must still parse, and as exactly one project
                    # whose key is the literal path.
                    self.assertEqual(self.projects(), {path: {"trust_level": "trusted"}})

    def test_never_writes_a_blanket_trust(self):
        self.assertEqual(self.seed("/Users/dev/proj").returncode, 0)
        projects = self.projects()
        self.assertEqual(len(projects), 1)
        self.assertNotIn("/", projects)
        self.assertNotIn("", projects)
        text = self.config.read_text()
        self.assertNotIn('"/"', text)
        self.assertNotIn("untrusted", text)

    def test_rerunning_does_not_duplicate_the_table(self):
        for _ in range(3):
            self.assertEqual(self.seed("/Users/dev/proj").returncode, 0)
        self.assertEqual(self.config.read_text().count("[projects."), 1)
        self.assertEqual(list(self.projects()), ["/Users/dev/proj"])

    def test_preserves_a_config_that_ends_with_a_table(self):
        self.config.write_text(
            self.CONFIG + '[model_providers.sandboxd]\nname = "Sandbox Proxy"\n'
        )
        self.assertEqual(self.seed("/Users/dev/proj").returncode, 0)
        parsed = tomllib.loads(self.config.read_text())
        self.assertEqual(parsed["model_providers"]["sandboxd"]["name"], "Sandbox Proxy")
        self.assertEqual(parsed["projects"]["/Users/dev/proj"]["trust_level"], "trusted")
        self.assertEqual(parsed["approval_policy"], "never")


if __name__ == "__main__":
    unittest.main()
