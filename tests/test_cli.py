import subprocess
import sys
import unittest
from pathlib import Path


CLI = Path(__file__).resolve().parents[1] / "krypper.py"


class CliTests(unittest.TestCase):
    def run_cli(self, *args):
        return subprocess.run(
            [sys.executable, str(CLI), *args],
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


if __name__ == "__main__":
    unittest.main()
