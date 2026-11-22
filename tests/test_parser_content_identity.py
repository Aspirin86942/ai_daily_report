"""Exercise the compiled v2 build scripts on private, disposable inputs."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
RUST = ROOT / "rust"
CREATE_FLAGS = getattr(subprocess, "CREATE_NO_WINDOW", 0)


def run(command, **kwargs):
    return subprocess.run(command, capture_output=True, text=True, encoding="utf-8",
                          errors="replace", creationflags=CREATE_FLAGS, **kwargs)


class ParserContentIdentityTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cargo = shutil.which("cargo") or str(Path.home() / ".cargo/bin/cargo.exe")
        cls.rustc = str(Path(cargo).with_name("rustc.exe" if os.name == "nt" else "rustc"))
        metadata = run([cargo, "metadata", "--no-deps", "--format-version", "1", "--locked"], cwd=RUST, check=True)
        packages = {p["id"]: p["name"] for p in json.loads(metadata.stdout)["packages"]}
        build = run([cargo, "build", "--locked", "--offline", "-p", "ai-daily-report",
                     "--lib", "--message-format", "json"], cwd=RUST)
        if build.returncode:
            messages = [json.loads(line) for line in build.stdout.splitlines() if line.startswith("{")]
            errors = [m["message"]["rendered"] for m in messages
                      if m.get("reason") == "compiler-message" and m["message"]["level"] == "error"]
            raise AssertionError("\n".join(errors) or build.stderr)
        cls.programs = {}
        for line in build.stdout.splitlines():
            event = json.loads(line)
            if event.get("reason") == "compiler-artifact" and "custom-build" in event["target"]["kind"]:
                name = packages.get(event["package_id"])
                if name in ("daily_parsers", "ai-daily-report"):
                    cls.programs[name] = Path(event["filenames"][0])
        if set(cls.programs) != {"daily_parsers", "ai-daily-report"}:
            raise AssertionError(f"Missing current build scripts: {list(cls.programs)}")

    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.rust = Path(temp.name) / "rust"
        self.rust.mkdir()
        self.env = {**os.environ, "RUSTC": self.rustc, "TARGET": "x86_64-pc-windows-msvc",
                    "PROFILE": "release", "CARGO_CFG_TARGET_FEATURE": "crt-static",
                    "CARGO_ENCODED_RUSTFLAGS": ""}
        self.env.pop("CARGO_FEATURE_OCR", None)
        self.inputs = json.loads((RUST / "parser-inputs.json").read_bytes())["sources"]
        for name in self.inputs:
            src, dst = RUST / name, self.rust / name
            dst.parent.mkdir(parents=True, exist_ok=True)
            if src.is_dir():
                shutil.copytree(src, dst)
            else:
                shutil.copyfile(src, dst)

    def script(self, name, **overrides):
        folder = "daily_report" if name == "ai-daily-report" else name
        return run([str(self.programs[name])], cwd=self.rust,
                   env={**self.env, "CARGO_MANIFEST_DIR": str(self.rust / folder), **overrides})

    def identity(self, **overrides):
        result = self.script("daily_parsers", **overrides)
        self.assertEqual(result.returncode, 0, result.stderr)
        digest = next(line.split("=", 2)[2] for line in result.stdout.splitlines()
                      if line.startswith("cargo:rustc-env=DAILY_PARSER_IDENTITY="))
        self.assertRegex(digest, "^[0-9a-f]{64}$")
        self.assertIn("cargo::metadata=identity=" + digest, result.stdout)
        return digest

    def test_parser_request_budget_and_pins_invalidate(self):
        baseline = self.identity()
        for name in ("daily_parsers/src/text/mod.rs", "daily_parsers/src/office/xlsx.rs",
                     "daily_parsers/src/pdf_wire.rs", "pdf_worker/src/reader.rs",
                     "daily_report/src/scan/parse.rs", "daily_report/src/config.defaults.json",
                     "Cargo.lock", "build-inputs.json", "daily_report/Cargo.toml"):
            with self.subTest(name=name):
                path = self.rust / name
                saved = path.read_bytes()
                path.write_bytes(saved + b"\n")
                self.assertNotEqual(self.identity(), baseline)
                path.write_bytes(saved)
        self.assertEqual(self.identity(), baseline)

    def test_abi_compiler_options_and_ocr_are_distinct(self):
        baseline = self.identity()
        for key, value in (("TARGET", "other-target"), ("PROFILE", "debug"),
                           ("CARGO_CFG_TARGET_FEATURE", "sse2"),
                           ("CARGO_ENCODED_RUSTFLAGS", "-Copt-level=2"), ("CARGO_FEATURE_OCR", "1")):
            with self.subTest(key=key):
                self.assertNotEqual(self.identity(**{key: value}), baseline)

    def test_report_only_changes_and_external_build_labels_do_not_change_parser(self):
        baseline = self.identity()
        path = self.rust / "daily_report/src/publish.rs"
        path.write_bytes(b"report-only formatting change")
        self.assertEqual(self.identity(AI_DAILY_ENGINE_BUILD="foreign-build"), baseline)

    def test_missing_or_nonlocal_input_fails_closed(self):
        path = self.rust / "parser-inputs.json"
        for name in ("missing.rs", "../outside.rs"):
            with self.subTest(name=name):
                path.write_text(json.dumps({"schema": "ai_daily_parser_inputs_v2", "sources": [name]}), encoding="utf-8")
                self.assertNotEqual(self.script("daily_parsers").returncode, 0)

    def test_engine_attests_to_its_parser_and_template_bytes(self):
        template = self.rust.parent / "templates/daily_prompt.md"
        template.parent.mkdir()
        template.write_text("one", encoding="utf-8")
        shutil.copyfile(RUST / "daily_report/build.rs", self.rust / "daily_report/build.rs")
        def engine(parser):
            result = self.script("ai-daily-report", DEP_DAILY_PARSERS_IDENTITY=parser)
            self.assertEqual(result.returncode, 0, result.stderr)
            return next(l for l in result.stdout.splitlines() if l.startswith("cargo:rustc-env=AI_DAILY_REPORT_BUILD="))
        parser = self.identity()
        baseline = engine(parser)
        self.assertNotEqual(engine("0" * 64), baseline)
        template.write_text("two", encoding="utf-8")
        self.assertNotEqual(engine(parser), baseline)


if __name__ == "__main__":
    unittest.main()
