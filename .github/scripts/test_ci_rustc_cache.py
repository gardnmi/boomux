"""The optional compiler cache must preserve normal Cargo execution paths."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
WRAPPER = ROOT / "scripts/rustc-cache.sh"


class RustcCacheTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="boomux cache test ")
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.record = self.root / "arguments.json"
        self.compiler = self.root / "compiler with spaces"
        self.write_program(self.compiler)
        self.env = {
            "PATH": str(self.root),
            "CACHE_TEST_RECORD": str(self.record),
        }
        self.arguments = ["a space", "$(not-a-command)", "semi;colon", "", "--cfg=feature=\"x\""]

    def write_program(self, path):
        path.write_text(
            f"#!{sys.executable}\n"
            "import json, os, sys\n"
            "from pathlib import Path\n"
            "Path(os.environ['CACHE_TEST_RECORD']).write_text(json.dumps(sys.argv))\n"
            "sys.exit(int(os.environ.get('CACHE_TEST_EXIT', '0')))\n"
        )
        path.chmod(0o755)

    def invoke(self):
        return subprocess.run(
            [str(WRAPPER), str(self.compiler), *self.arguments],
            env=self.env, capture_output=True, text=True, check=False,
        )

    def assert_compiler_passthrough(self):
        result = self.invoke()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(self.record.read_text()), [str(self.compiler), *self.arguments])

    def test_missing_cache_preserves_compiler_arguments(self):
        self.assert_compiler_passthrough()

    def test_local_cache_receives_exact_compiler_and_arguments(self):
        cache = self.root / "kache"
        self.write_program(cache)
        result = self.invoke()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(self.record.read_text()), [str(cache), str(self.compiler), *self.arguments])

    def test_ci_and_explicit_disable_bypass_installed_cache(self):
        self.write_program(self.root / "kache")
        for key, value in [("CI", "true"), ("KACHE_DISABLED", "1"), ("KACHE_DISABLED", "true")]:
            with self.subTest(key=key, value=value):
                self.env[key] = value
                self.assert_compiler_passthrough()
                del self.env[key]

    def test_cache_failure_is_not_retried_as_uncached_success(self):
        cache = self.root / "kache"
        self.write_program(cache)
        self.env["CACHE_TEST_EXIT"] = "23"
        self.assertEqual(self.invoke().returncode, 23)
        self.assertEqual(json.loads(self.record.read_text())[0], str(cache))

    def test_compiler_failure_propagates_without_cache(self):
        self.env["CACHE_TEST_EXIT"] = "19"
        self.assertEqual(self.invoke().returncode, 19)


if __name__ == "__main__":
    unittest.main()
