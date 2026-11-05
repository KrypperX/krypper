import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


CLI = Path(__file__).resolve().parents[1] / "krypper.py"


class CliTests(unittest.TestCase):
    def run_cli(self, *args, cwd=None):
        return subprocess.run(
            [sys.executable, str(CLI), *args],
            cwd=cwd,
            capture_output=True,
            text=True,
            check=False,
        )

    def test_help_prints_usage_and_succeeds(self):
        result = self.run_cli("--help")

        self.assertEqual(result.returncode, 0)
        self.assertIn("usage: krypper", result.stdout)

    def test_unsupported_argument_prints_error_and_usage(self):
        result = self.run_cli("--unsupported")

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("usage: krypper", result.stderr)
        self.assertIn("error: unrecognized arguments: --unsupported", result.stderr)

    def test_show_prints_utf8_file_contents(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "note.txt"
            path.write_text("Hello, café.\n", encoding="utf-8")

            result = self.run_cli("show", "note.txt", cwd=directory)

        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "Hello, café.\n")
        self.assertEqual(result.stderr, "")

    def test_show_rejects_absolute_path(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "note.txt"
            path.write_text("text", encoding="utf-8")

            result = self.run_cli("show", str(path), cwd=directory)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("absolute paths are not allowed", result.stderr)

    def test_show_rejects_path_outside_current_directory(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "outside.txt").write_text("text", encoding="utf-8")
            inside = root / "inside"
            inside.mkdir()

            result = self.run_cli("show", "../outside.txt", cwd=inside)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("path must stay inside the current directory", result.stderr)

    def test_show_rejects_symlink_escape(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            outside = root / "outside.txt"
            outside.write_text("text", encoding="utf-8")
            inside = root / "inside"
            inside.mkdir()
            link = inside / "escape.txt"
            try:
                link.symlink_to(outside)
            except (NotImplementedError, OSError) as error:
                self.skipTest(f"symlinks are unavailable: {error}")

            result = self.run_cli("show", "escape.txt", cwd=inside)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("path must stay inside the current directory", result.stderr)

    def test_show_rejects_missing_file(self):
        with tempfile.TemporaryDirectory() as directory:
            result = self.run_cli("show", "missing.txt", cwd=directory)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("file not found", result.stderr)

    def test_show_rejects_directory(self):
        with tempfile.TemporaryDirectory() as directory:
            Path(directory, "folder").mkdir()

            result = self.run_cli("show", "folder", cwd=directory)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not a regular file", result.stderr)

    def test_show_rejects_invalid_utf8(self):
        with tempfile.TemporaryDirectory() as directory:
            Path(directory, "invalid.txt").write_bytes(b"\xff")

            result = self.run_cli("show", "invalid.txt", cwd=directory)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not valid UTF-8", result.stderr)

    def test_show_rejects_file_larger_than_one_mib(self):
        with tempfile.TemporaryDirectory() as directory:
            Path(directory, "large.txt").write_bytes(b"x" * (1024 * 1024 + 1))

            result = self.run_cli("show", "large.txt", cwd=directory)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("file exceeds 1 MiB", result.stderr)

    def test_show_accepts_file_at_one_mib_limit(self):
        with tempfile.TemporaryDirectory() as directory:
            contents = "x" * (1024 * 1024)
            Path(directory, "large.txt").write_text(contents, encoding="utf-8")

            result = self.run_cli("show", "large.txt", cwd=directory)

        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stdout, contents)
        self.assertEqual(result.stderr, "")


if __name__ == "__main__":
    unittest.main()
