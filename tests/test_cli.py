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
    def run_cli(self, *args, cwd=None, input=None, text=True):
        return subprocess.run(
            [sys.executable, str(CLI), *args],
            cwd=cwd,
            input=input,
            capture_output=True,
            text=text,
            check=False,
        )

    def test_help_prints_usage_and_succeeds(self):
        result = self.run_cli("--help")

        self.assertEqual(result.returncode, 0)
        self.assertIn("usage: krypper", result.stdout)
        self.assertIn("{show,list,write}", result.stdout)

    def test_missing_command_prints_usage_and_fails(self):
        result = self.run_cli()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("usage: krypper", result.stderr)
        self.assertIn("{show,list,write}", result.stderr)

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

    def test_write_creates_utf8_file(self):
        with tempfile.TemporaryDirectory() as directory:
            result = self.run_cli("write", "note.txt", "Hello, café.", cwd=directory)

            self.assertEqual(Path(directory, "note.txt").read_text(encoding="utf-8"), "Hello, café.")

        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stderr, "")

    def test_write_stdin_creates_multiline_utf8_file(self):
        with tempfile.TemporaryDirectory() as directory:
            contents = "First line\nCafé 東京\n"

            result = self.run_cli("write", "--stdin", "note.txt", cwd=directory, input=contents)

            self.assertEqual(Path(directory, "note.txt").read_text(encoding="utf-8"), contents)

        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stderr, "")

    def test_write_stdin_accepts_exactly_one_mibibyte(self):
        with tempfile.TemporaryDirectory() as directory:
            contents = "é" * (1024 * 1024 // 2)

            result = self.run_cli("write", "--stdin", "limit.txt", cwd=directory, input=contents)

            self.assertEqual(Path(directory, "limit.txt").stat().st_size, 1024 * 1024)

        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stderr, "")

    def test_write_stdin_rejects_over_limit_without_creating_file(self):
        with tempfile.TemporaryDirectory() as directory:
            contents = "x" * (1024 * 1024 + 1)

            result = self.run_cli("write", "--stdin", "too-large.txt", cwd=directory, input=contents)

            self.assertFalse(Path(directory, "too-large.txt").exists())

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("stdin exceeds 1 MiB", result.stderr)

    def test_write_stdin_rejects_invalid_utf8_without_creating_file(self):
        with tempfile.TemporaryDirectory() as directory:
            result = self.run_cli("write", "--stdin", "invalid.txt", cwd=directory, input=b"bad \xff", text=False)

            self.assertFalse(Path(directory, "invalid.txt").exists())

        self.assertNotEqual(result.returncode, 0)
        self.assertIn(b"stdin is not valid UTF-8", result.stderr)

    def test_write_creates_file_in_existing_nested_parent(self):
        with tempfile.TemporaryDirectory() as directory:
            Path(directory, "nested").mkdir()

            result = self.run_cli("write", "nested/note.txt", "nested text", cwd=directory)

            self.assertEqual(Path(directory, "nested", "note.txt").read_text(encoding="utf-8"), "nested text")

        self.assertEqual(result.returncode, 0)

    def test_write_accepts_dash_leading_text_and_unquoted_words(self):
        with tempfile.TemporaryDirectory() as directory:
            result = self.run_cli("write", "note.txt", "-draft", "with", "spaces", cwd=directory)

            self.assertEqual(Path(directory, "note.txt").read_text(encoding="utf-8"), "-draft with spaces")

        self.assertEqual(result.returncode, 0)

    def test_write_refuses_to_replace_existing_file(self):
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory, "note.txt")
            target.write_text("keep this", encoding="utf-8")

            result = self.run_cli("write", "note.txt", "replacement", cwd=directory)

            self.assertEqual(target.read_text(encoding="utf-8"), "keep this")

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("file already exists", result.stderr)

    def test_write_rejects_absolute_and_outside_paths(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "workspace"
            root.mkdir()
            outside = Path(directory) / "outside.txt"
            outside.write_text("keep", encoding="utf-8")
            absolute = self.run_cli("write", str(outside), "replacement", cwd=root)
            traversal = self.run_cli("write", "../outside.txt", "replacement", cwd=root)

            self.assertEqual(outside.read_text(encoding="utf-8"), "keep")
            self.assertFalse((root / "outside.txt").exists())

        self.assertNotEqual(absolute.returncode, 0)
        self.assertIn("absolute paths are not allowed", absolute.stderr)
        self.assertNotEqual(traversal.returncode, 0)
        self.assertIn("path must stay inside the current directory", traversal.stderr)

    def test_write_rejects_symlink_parent_and_target(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            nested = root / "nested"
            nested.mkdir()
            target = nested / "note.txt"
            target.write_text("keep", encoding="utf-8")
            parent_link = root / "linked"
            target_link = root / "alias.txt"
            try:
                parent_link.symlink_to(nested, target_is_directory=True)
                target_link.symlink_to(target)
            except (NotImplementedError, OSError) as error:
                self.skipTest(f"symlinks are unavailable: {error}")

            parent_result = self.run_cli("write", "linked/new.txt", "text", cwd=directory)
            target_result = self.run_cli("write", "alias.txt", "replacement", cwd=directory)

            self.assertEqual(target.read_text(encoding="utf-8"), "keep")
            self.assertTrue(target_link.is_symlink())
            self.assertFalse((nested / "new.txt").exists())

        self.assertNotEqual(parent_result.returncode, 0)
        self.assertIn("symlink paths are not allowed", parent_result.stderr)
        self.assertNotEqual(target_result.returncode, 0)
        self.assertIn("file already exists", target_result.stderr)

    def test_write_rejects_parent_swapped_to_symlink_before_open(self):
        with tempfile.TemporaryDirectory() as directory, tempfile.TemporaryDirectory() as outside:
            root = Path(directory)
            (root / "nested").mkdir()
            previous_directory = Path.cwd()
            os.chdir(root)
            original_open = os.open
            swapped = False

            def swap_parent_before_open(path, flags, *args, **kwargs):
                nonlocal swapped
                if path == "nested" and kwargs.get("dir_fd") is not None and not swapped:
                    os.rename("nested", "moved")
                    os.symlink(outside, "nested", target_is_directory=True)
                    swapped = True
                return original_open(path, flags, *args, **kwargs)

            try:
                with patch("krypper.os.open", side_effect=swap_parent_before_open):
                    with self.assertRaisesRegex(ValueError, "invalid parent directory"):
                        krypper.create_text_file("nested/new.txt", "text")
            finally:
                os.chdir(previous_directory)

            self.assertTrue(swapped)
            self.assertFalse(Path(outside, "new.txt").exists())
            self.assertFalse(Path(directory, "moved", "new.txt").exists())

    def test_write_rejects_missing_and_non_directory_parents(self):
        with tempfile.TemporaryDirectory() as directory:
            Path(directory, "file.txt").touch()
            missing = self.run_cli("write", "missing/note.txt", "text", cwd=directory)
            not_directory = self.run_cli("write", "file.txt/note.txt", "text", cwd=directory)

            self.assertFalse(Path(directory, "missing").exists())

        self.assertNotEqual(missing.returncode, 0)
        self.assertIn("parent directory not found", missing.stderr)
        self.assertNotEqual(not_directory.returncode, 0)
        self.assertIn("parent is not a directory", not_directory.stderr)

    def test_write_enforces_encoded_one_mib_limit_without_creating_file(self):
        with tempfile.TemporaryDirectory() as directory:
            previous_directory = Path.cwd()
            os.chdir(directory)
            try:
                krypper.create_text_file("limit.txt", "é" * (1024 * 1024 // 2))
                with self.assertRaisesRegex(ValueError, "text exceeds 1 MiB"):
                    krypper.create_text_file("too-large.txt", "é" * (1024 * 1024 // 2 + 1))
            finally:
                os.chdir(previous_directory)

            self.assertEqual(Path(directory, "limit.txt").stat().st_size, 1024 * 1024)
            self.assertFalse(Path(directory, "too-large.txt").exists())

    def test_write_removes_partial_file_when_write_fails(self):
        with tempfile.TemporaryDirectory() as directory:
            previous_directory = Path.cwd()
            os.chdir(directory)
            original_write = os.write
            writes = 0

            def fail_after_partial_write(fd, contents):
                nonlocal writes
                writes += 1
                if writes == 1:
                    return original_write(fd, contents[:3])
                raise OSError("disk full")

            try:
                with patch("krypper.os.write", side_effect=fail_after_partial_write):
                    with self.assertRaisesRegex(ValueError, "unable to write file"):
                        krypper.create_text_file("partial.txt", "complete text")
            finally:
                os.chdir(previous_directory)

            self.assertFalse(Path(directory, "partial.txt").exists())


if __name__ == "__main__":
    unittest.main()
