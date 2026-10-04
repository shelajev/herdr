"""Offline behavioral coverage for the real curl/zig prefetch shell script."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("fetch-zig-deps.sh")


def zon(*deps):
    return '.{ .name = .fixture, .paths = .{""}, .dependencies = .{\n' + ",\n".join(
        f'.dep{i} = .{{ {dep} }}' for i, dep in enumerate(deps)
    ) + '\n} }\n'


def dependency(name, lazy=False):
    return (
        f'.url = "https://example.test/{name}.tar.gz", '
        f'.hash = "hash-{name}", .lazy = {str(lazy).lower()},'
    )


class FetchZigDepsTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="fetch-zig-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.vendor = self.root / "vendor/libghostty-vt"
        self.vendor.mkdir(parents=True)
        (self.vendor / "build.zig").write_text("// shim requires this cwd\n")
        (self.root / "scripts").mkdir()
        shutil.copyfile(SCRIPT, self.root / "scripts/fetch-zig-deps.sh")
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.cache = self.root / "cache"
        self.calls = self.root / "calls.jsonl"
        self.config = self.root / "config.json"
        self.write_shim("curl", '''
args = sys.argv[1:]
url = args[1]
record("curl", url)
name = url.rsplit("/", 1)[-1].removesuffix(".tar.gz")
if name in config.get("download_fail", []):
    sys.exit(22)
Path(args[args.index("-o") + 1]).write_text(name)
''')
        self.write_shim("zig", '''
if sys.argv[1] == "env":
    if config.get("json_env"):
        print(json.dumps({"global_cache_dir": os.environ["TEST_CACHE"]}))
    else:
        print('.{ .global_cache_dir = ' + json.dumps(os.environ["TEST_CACHE"]) + ', }')
    sys.exit(0)
assert sys.argv[1] == "fetch"
assert Path.cwd() == Path(os.environ["TEST_VENDOR"])
assert Path("build.zig").exists()
archive = Path(sys.argv[2])
assert archive.name.endswith((".tar.gz", ".tar.xz", ".tar.zst"))
name = archive.read_text()
record("fetch", name)
if name in config.get("import_fail", []):
    sys.exit(1)
if name in config.get("transitive", {}):
    package = Path(os.environ["TEST_CACHE"]) / "p" / ("hash-" + name)
    package.mkdir(parents=True, exist_ok=True)
    (package / "build.zig.zon").write_text(config["transitive"][name])
print(config.get("hashes", {}).get(name, "hash-" + name))
''')

    def write_shim(self, name, body):
        path = self.bin / name
        path.write_text('''#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
config = json.loads(Path(os.environ["TEST_CONFIG"]).read_text())
def record(kind, value):
    with open(os.environ["TEST_CALLS"], "a") as f:
        f.write(json.dumps([kind, value]) + "\\n")
''' + body)
        path.chmod(0o755)

    def run_fetch(self, manifest=None, **config):
        if manifest is None:
            manifest = zon(dependency("required"), dependency("lazy", lazy=True))
        (self.vendor / "build.zig.zon").write_text(manifest)
        self.config.write_text(json.dumps(config))
        env = dict(os.environ, PATH=f"{self.bin}:{os.environ['PATH']}",
                   TEST_CONFIG=str(self.config), TEST_CALLS=str(self.calls),
                   TEST_CACHE=str(self.cache), TEST_VENDOR=str(self.vendor))
        env.pop("ZIG_GLOBAL_CACHE_DIR", None)
        result = subprocess.run(["bash", str(self.root / "scripts/fetch-zig-deps.sh")],
                                env=env, text=True, capture_output=True, timeout=20)
        calls = [json.loads(line) for line in self.calls.read_text().splitlines()] if self.calls.exists() else []
        self.fetches = [value for kind, value in calls if kind == "fetch"]
        self.urls = [value for kind, value in calls if kind == "curl"]
        self.output = result.stdout + result.stderr
        return result.returncode

    def test_matching_hashes(self):
        self.assertEqual(self.run_fetch(), 0, self.output)
        self.assertCountEqual(self.fetches, ["required", "lazy"])
        self.assertIn("required ok=1 failed=0; optional ok=1 failed=0", self.output)

    def test_required_download_failure(self):
        self.assertNotEqual(self.run_fetch(download_fail=["required"]), 0)
        self.assertEqual(self.fetches, ["lazy"])

    def test_lazy_download_failure(self):
        self.assertEqual(self.run_fetch(download_fail=["lazy"]), 0, self.output)
        self.assertEqual(self.fetches, ["required"])
        self.assertIn("optional (lazy) not fetched:", self.output)

    def test_required_hash_mismatch(self):
        self.assertNotEqual(self.run_fetch(hashes={"required": "wrong"}), 0)
        self.assertCountEqual(self.fetches, ["required", "lazy"])

    def test_lazy_hash_mismatch(self):
        self.assertEqual(self.run_fetch(hashes={"lazy": "wrong"}), 0, self.output)
        self.assertCountEqual(self.fetches, ["required", "lazy"])
        self.assertIn("optional (lazy) not fetched:", self.output)

    def test_required_import_failure(self):
        self.assertNotEqual(self.run_fetch(import_fail=["required"]), 0)
        self.assertCountEqual(self.fetches, ["required", "lazy"])

    def test_lazy_import_failure(self):
        self.assertEqual(self.run_fetch(import_fail=["lazy"]), 0, self.output)
        self.assertCountEqual(self.fetches, ["required", "lazy"])
        self.assertIn("optional (lazy) not fetched:", self.output)

    def test_later_required_reference_cannot_hide_behind_lazy_failure(self):
        self.assertNotEqual(self.run_fetch(
            transitive={"required": zon(dependency("lazy"))},
            download_fail=["lazy"]), 0)
        self.assertEqual(self.fetches, ["required"])
        self.assertEqual(self.urls.count("https://example.test/lazy.tar.gz"), 2)

    def test_transitive_required_download_failure(self):
        self.assertNotEqual(self.run_fetch(
            transitive={"required": zon(dependency("child"))}, download_fail=["child"]), 0)
        self.assertCountEqual(self.fetches, ["required", "lazy"])
        self.assertIn("https://example.test/child.tar.gz", self.urls)

    def test_three_passes(self):
        self.assertEqual(self.run_fetch(transitive={
            "required": zon(dependency("child")),
            "child": zon(dependency("grandchild")),
        }), 0, self.output)
        self.assertCountEqual(self.fetches, ["required", "lazy", "child", "grandchild"])

    def test_git_suffix_codeload_rewrite(self):
        manifest = zon('.url = "git+https://github.com/owner/libvaxis.git#abcdef", .hash = "hash-abcdef",')
        self.assertEqual(self.run_fetch(manifest), 0, self.output)
        self.assertEqual(self.fetches, ["abcdef"])
        self.assertEqual(self.urls, ["https://codeload.github.com/owner/libvaxis/tar.gz/abcdef"])

    def test_comments_and_field_order(self):
        manifest = zon('''// .lazy = true, .url = "https://ignored.test/a"
            .hash = "hash-required", // } fake closing brace
            .url =
                "https://example.test/required.tar.gz",
        ''', '''.lazy = true,
            .hash = "hash-lazy", .url = "https://example.test/lazy.tar.gz",
        ''')
        self.assertNotEqual(self.run_fetch(manifest, download_fail=["required"]), 0)
        self.assertEqual(self.fetches, ["lazy"])
        self.assertEqual(len(self.urls), 2)

    def test_required_reference_wins(self):
        self.assertNotEqual(self.run_fetch(
            zon(dependency("shared", True), dependency("shared")),
            download_fail=["shared"]), 0)
        self.assertEqual(self.fetches, [])
        self.assertEqual(len(self.urls), 1)

    def test_invalid_manifest_fails_closed(self):
        self.assertNotEqual(self.run_fetch('.{ .dependencies = .{'), 0)
        self.assertEqual(self.fetches, [])

    def test_missing_hash_fails_closed(self):
        self.assertNotEqual(self.run_fetch(zon('.url = "https://example.test/required.tar.gz",')), 0)
        self.assertEqual(self.fetches, [])

    def test_json_zig_env(self):
        self.assertEqual(self.run_fetch(json_env=True), 0, self.output)
        self.assertCountEqual(self.fetches, ["required", "lazy"])

    def test_unsupported_required_git_host(self):
        self.assertNotEqual(self.run_fetch(zon(
            '.url = "git+https://example.test/repo#abcdef", .hash = "hash-abcdef",')), 0)
        self.assertEqual(self.fetches, [])
        self.assertEqual(self.urls, [])


if __name__ == "__main__":
    unittest.main()
