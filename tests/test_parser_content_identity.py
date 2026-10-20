"""Run the real Rust build script against disposable source trees."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
RUST = ROOT / "rust"
CARGO = Path.home() / ".cargo/bin/cargo.exe"


class ParserContentIdentityTests(unittest.TestCase):
    def setUp(self):
        builds = list((RUST / "target/debug/build").glob("ai-daily-scanner-core-*/build-script-build.exe"))
        self.assertTrue(builds, "Build debug workers first")
        self.program = max(builds, key=lambda p: p.stat().st_mtime_ns)
        outputs = list((RUST / "target/debug/build").glob("ai-daily-scanner-core-*/out/parser-build.json"))
        document = json.loads(max(outputs, key=lambda p: p.stat().st_mtime_ns).read_bytes())
        self.temp = tempfile.TemporaryDirectory(prefix="parser-identity-")
        self.root = Path(self.temp.name)
        for path in RUST.iterdir():
            if path.is_file() and path.suffix in (".toml", ".lock", ".rs", ".json"):
                shutil.copy2(path, self.root / path.name)
            elif path.is_dir() and (path / "Cargo.toml").is_file():
                target = self.root / path.name
                target.mkdir()
                shutil.copy2(path / "Cargo.toml", target / "Cargo.toml")
                for name in ("src", "tests", "examples"):
                    if (path / name).exists():
                        shutil.copytree(path / name, target / name)
                if (path / "build.rs").exists():
                    shutil.copy2(path / "build.rs", target / "build.rs")
        self.environment = dict(os.environ)
        for name in list(self.environment):
            if name.startswith("CARGO_CFG_"):
                del self.environment[name]
        values = dict(document["environment"])
        self.environment["CARGO_ENCODED_RUSTFLAGS"] = values.pop("rustflags")
        self.environment.update(values)
        self.environment.update({
            "CARGO": str(CARGO), "RUSTC": str(CARGO.with_name("rustc.exe")),
            "CARGO_MANIFEST_DIR": str(self.root / "scanner_core"),
            "OUT_DIR": str(self.root / "out"), "TARGET": "x86_64-pc-windows-msvc",
        })
        (self.root / "out").mkdir()
        self.baseline = self.identity()

    def tearDown(self):
        self.temp.cleanup()

    def identity(self):
        result = subprocess.run([str(self.program)], cwd=self.root, env=self.environment,
                                capture_output=True, text=True, creationflags=0x08000000)
        self.assertEqual(result.returncode, 0, result.stderr)
        return dict(line.removeprefix("cargo:rustc-env=").split("=", 1)
                    for line in result.stdout.splitlines() if line.startswith("cargo:rustc-env="))

    def test_orchestration_and_debug_metadata_do_not_invalidate_content(self):
        for name in ("scanner_core/src/scheduler.rs", "scanner_core/src/store/cache.rs",
                     "report_app/src/run.rs"):
            with (self.root / name).open("a", encoding="utf-8") as stream:
                stream.write("\n// Isolated non-parser edit.\n")
        with (self.root / "Cargo.toml").open("a", encoding="utf-8") as stream:
            stream.write("\n[profile.dev]\ndebug = 1\n")
        self.environment["CARGO_ENCODED_RUSTFLAGS"] = "\x1f".join(filter(None, [
            self.environment["CARGO_ENCODED_RUSTFLAGS"], "-C", "debuginfo=1"]))
        candidate = self.identity()
        self.assertEqual(candidate["AI_DAILY_PARSER_BUILD"], self.baseline["AI_DAILY_PARSER_BUILD"])
        self.assertNotEqual(candidate["AI_DAILY_ENGINE_BUILD"], self.baseline["AI_DAILY_ENGINE_BUILD"])

    def test_parser_protocol_budget_and_dependency_features_invalidate(self):
        for name in ("text_parser/src/lib.rs", "worker_contract/src/lib.rs",
                     "scanner_core/src/config.rs", "pdf_parser/src/lib.rs", "build-inputs.json",
                     "report_app/src/archive/parse_request.rs"):
            path = self.root / name
            original = path.read_bytes()
            path.write_bytes(original + b"\n ")
            self.assertNotEqual(self.identity()["AI_DAILY_PARSER_BUILD"], self.baseline["AI_DAILY_PARSER_BUILD"], name)
            path.write_bytes(original)
        path = self.root / "Cargo.toml"
        path.write_text(path.read_text(encoding="utf-8").replace('["arbitrary_precision"]', '["arbitrary_precision", "raw_value"]'), encoding="utf-8")
        self.assertNotEqual(self.identity()["AI_DAILY_PARSER_BUILD"], self.baseline["AI_DAILY_PARSER_BUILD"])

    def test_build_label_cannot_override_parser_identity(self):
        self.environment["AI_DAILY_BUILD_ID"] = "explicit-audit-label"
        candidate = self.identity()
        self.assertEqual(candidate["AI_DAILY_ENGINE_BUILD"], "explicit-audit-label")
        self.assertEqual(candidate["AI_DAILY_PARSER_BUILD"], self.baseline["AI_DAILY_PARSER_BUILD"])

    def test_application_manifest_can_change_unified_parser_features(self):
        path = self.root / "report_app/Cargo.toml"
        original = path.read_text(encoding="utf-8")
        needle = "serde_json.workspace = true"
        self.assertIn(needle, original)
        path.write_text(original.replace(
            needle, 'serde_json = { workspace = true, features = ["raw_value"] }',
        ), encoding="utf-8")
        self.assertNotEqual(
            self.identity()["AI_DAILY_PARSER_BUILD"], self.baseline["AI_DAILY_PARSER_BUILD"],
        )

    def test_corrupted_dependency_cache_is_recomputed(self):
        cache = self.root / "out/parser-dependencies.json"
        if not cache.exists():
            self.skipTest("Cargo configuration overrides disable metadata caching")
        saved = json.loads(cache.read_bytes())
        saved["value"]["metadata"]["resolve"]["nodes"] = []
        cache.write_text(json.dumps(saved), encoding="utf-8")
        self.assertEqual(self.identity(), self.baseline)
        restored = json.loads(cache.read_bytes())
        self.assertTrue(restored["value"]["metadata"]["resolve"]["nodes"])

    def test_stale_build_script_cannot_attest_to_changed_helper_source(self):
        path = self.root / "build_identity/src/parser_identity.rs"
        path.write_bytes(path.read_bytes() + b"\n ")
        result = subprocess.run(
            [str(self.program)], cwd=self.root, env=self.environment,
            capture_output=True, text=True, creationflags=0x08000000,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("build script does not match source", result.stderr)
        self.assertNotIn("cargo:rustc-env=AI_DAILY_PARSER_BUILD=", result.stdout)


if __name__ == "__main__":
    unittest.main()
