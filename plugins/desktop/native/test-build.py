#!/usr/bin/env python3
"""Compiler bootstrap checks; no compiler download, registration or notification."""
import importlib.util
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location("desktop_build", Path(__file__).with_name("build.py"))
build = importlib.util.module_from_spec(spec)
spec.loader.exec_module(build)


class BatchEnvironmentTests(unittest.TestCase):
    def test_cmd_receives_literal_quotes_not_msvcrt_backslashes(self):
        comspec = r"C:\Windows\System32\cmd.exe"
        batch = r"C:\Program Files\Microsoft Visual Studio\Common7\Tools\VsDevCmd.bat"
        result = subprocess.CompletedProcess([], 0, "PATH=C:\\工具\\bin\r\n".encode("utf-16-le"), b"")
        with mock.patch.object(build.subprocess, "run", return_value=result) as run:
            self.assertEqual(build.batch_environment(comspec, batch, {}), "PATH=C:\\工具\\bin\r\n")
        command = run.call_args.args[0]
        self.assertIsInstance(command, str)
        self.assertIn(' /s /c "call "' + batch + '" ', command)
        self.assertNotIn('\\"', command)
        self.assertTrue(command.endswith(' >nul && set"'))
        self.assertEqual(run.call_args.kwargs["executable"], comspec)

    def test_failure_preserves_stderr_without_environment_dump(self):
        for diagnostic in (b"fixture vcvars failure", "fixture vcvars failure".encode("utf-16-le")):
            result = subprocess.CompletedProcess([], 23, "PRIVATE_ENV_CANARY=value".encode("utf-16-le"), diagnostic)
            with mock.patch.object(build.subprocess, "run", return_value=result):
                with self.assertRaises(SystemExit) as error:
                    build.batch_environment("cmd.exe", r"C:\Program Files\VsDevCmd.bat", {})
            self.assertIn("exit 23", str(error.exception))
            self.assertIn("fixture vcvars failure", str(error.exception))
            self.assertNotIn("PRIVATE_ENV_CANARY", str(error.exception))

    def test_invalid_path_is_rejected_before_process_creation(self):
        with mock.patch.object(build.subprocess, "run") as run:
            with self.assertRaises(SystemExit):
                build.batch_environment("cmd.exe", 'C:\\tools\\x&set.bat', {})
            run.assert_not_called()

    @unittest.skipUnless(os.name == "nt", "actual cmd.exe check requires Windows")
    def test_real_cmd_batch_path_with_spaces_and_failure(self):
        comspec = os.environ.get("COMSPEC", r"C:\Windows\System32\cmd.exe")
        with tempfile.TemporaryDirectory(prefix="zcalld batch ") as directory:
            batch = Path(directory) / "fake VsDevCmd.bat"
            batch.write_bytes(b"@echo off\r\nset ZCALLD_BATCH_FIXTURE=ready\r\nexit /b 0\r\n")
            result = build.batch_environment(comspec, batch, dict(os.environ))
            self.assertIn("ZCALLD_BATCH_FIXTURE=ready", result.splitlines())
            batch.write_bytes(b"@echo off\r\necho fixture vcvars failure 1>&2\r\nexit /b 23\r\n")
            with self.assertRaises(SystemExit) as error:
                build.batch_environment(comspec, batch, dict(os.environ))
            self.assertIn("fixture vcvars failure", str(error.exception))
            self.assertIn("exit 23", str(error.exception))


if __name__ == "__main__":
    unittest.main()
