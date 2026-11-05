import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import krypper


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
        self.assertIn("{show,list}", result.stdout)

    def test_missing_command_prints_usage_and_fails(self):
        result = self.run_cli()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("usage: krypper", result.stderr)
        self.assertIn("{show,list}", result.stderr)

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

    def test_show_accepts_symlink_to_file_inside_current_directory(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "note.txt").write_text("inside\n", encoding="utf-8")
            link = root / "alias.txt"
            try:
                link.symlink_to(root / "note.txt")
            except (NotImplementedError, OSError) as error:
                self.skipTest(f"symlinks are unavailable: {error}")

            result = self.run_cli("show", "alias.txt", cwd=directory)

        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "inside\n")

    def test_show_rejects_file_replaced_by_outside_symlink_before_open(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            selected = root / "note.txt"
            selected.write_text("inside\n", encoding="utf-8")
            outside = root.parent / f"{root.name}-outside.txt"
            outside.write_text("outside\n", encoding="utf-8")
            original_open = os.open
            replaced = False

            def replace_before_open(path, *args, **kwargs):
                nonlocal replaced
                if not replaced and os.fspath(path) in (str(selected), "note.txt"):
                    replaced = True
                    selected.unlink()
                    selected.symlink_to(outside)
                return original_open(path, *args, **kwargs)

            previous_directory = Path.cwd()
            os.chdir(root)
            try:
                with patch("krypper.os.open", side_effect=replace_before_open):
                    with self.assertRaisesRegex(ValueError, "unable to read file"):
                        krypper.read_text_file("note.txt")
            finally:
                os.chdir(previous_directory)
                outside.unlink(missing_ok=True)

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

    def test_list_defaults_to_current_directory_and_includes_hidden_entries(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name in ("zeta", ".hidden", "alpha"):
                (root / name).touch()

            result = self.run_cli("list", cwd=directory)

        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stdout.splitlines(), [json.dumps(name) for name in (".hidden", "alpha", "zeta")])
        self.assertEqual(result.stderr, "")

    def test_list_selected_nested_directory_is_not_recursive(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            selected = root / "nested"
            selected.mkdir()
            (selected / "file.txt").touch()
            (selected / "child").mkdir()
            (selected / "child" / "deeper.txt").touch()

            (selected / "file.txt").write_text("entry contents", encoding="utf-8")
            result = self.run_cli("list", "nested/", cwd=directory)

        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stdout.splitlines(), ['"child"', '"file.txt"'])
        self.assertNotIn("entry contents", result.stdout)

    def test_list_output_is_deterministic_and_escapes_control_characters(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            names = ("z-last", "line\nbreak", "a-first", "tab\tname")
            for name in names:
                (root / name).touch()

            first = self.run_cli("list", cwd=directory)
            second = self.run_cli("list", cwd=directory)

        expected_names = sorted(names)
        expected = [json.dumps(name) for name in expected_names]
        self.assertEqual(first.returncode, 0)
        self.assertEqual(first.stdout, "".join(f"{line}\n" for line in expected))
        self.assertEqual(second.stdout, first.stdout)

    def test_list_rejects_symlink_escape(self):
        with tempfile.TemporaryDirectory() as directory, tempfile.TemporaryDirectory() as outside:
            link = Path(directory) / "escape"
            try:
                link.symlink_to(outside, target_is_directory=True)
            except (NotImplementedError, OSError) as error:
                self.skipTest(f"symlinks are unavailable: {error}")

            result = self.run_cli("list", "escape", cwd=directory)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("path must stay inside the current directory", result.stderr)

    def test_list_does_not_follow_symlink_entries(self):
        with tempfile.TemporaryDirectory() as directory, tempfile.TemporaryDirectory() as outside:
            root = Path(directory)
            (Path(outside) / "secret.txt").touch()
            try:
                (root / "linked-directory").symlink_to(outside, target_is_directory=True)
            except (NotImplementedError, OSError) as error:
                self.skipTest(f"symlinks are unavailable: {error}")

            result = self.run_cli("list", cwd=directory)

        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stdout.splitlines(), ['"linked-directory"'])
        self.assertNotIn("secret.txt", result.stdout)

    def test_list_rejects_missing_directory(self):
        with tempfile.TemporaryDirectory() as directory:
            result = self.run_cli("list", "missing", cwd=directory)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("file not found", result.stderr)

    def test_list_rejects_non_directory(self):
        with tempfile.TemporaryDirectory() as directory:
            Path(directory, "file.txt").touch()
            result = self.run_cli("list", "file.txt", cwd=directory)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not a directory", result.stderr)

    def test_list_rejects_absolute_path(self):
        with tempfile.TemporaryDirectory() as directory:
            result = self.run_cli("list", str(Path(directory) / "child"), cwd=directory)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("absolute paths are not allowed", result.stderr)

    def test_list_rejects_path_outside_current_directory(self):
        with tempfile.TemporaryDirectory() as directory:
            result = self.run_cli("list", "..", cwd=directory)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("path must stay inside the current directory", result.stderr)


if __name__ == "__main__":
    unittest.main()
