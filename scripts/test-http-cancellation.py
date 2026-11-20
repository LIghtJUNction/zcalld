#!/usr/bin/env python3
"""HTTP extension cancellation on confirmed disconnects; keep half-closed clients working."""
import argparse
import contextlib
import http.client
import importlib.util
import json
import os
from pathlib import Path
import shutil
import socket
import struct
import time
import unittest

spec = importlib.util.spec_from_file_location(
    "codex_metadata_fixture", Path(__file__).with_name("test-codex-metadata.py"))
metadata_fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(metadata_fixture)


class HttpCancellationTests(metadata_fixture.CodexMetadataTests):
    def connection(self, unix=False):
        if unix:
            conn = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            conn.settimeout(5)
            conn.connect(str(self.root / "http.sock"))
            return conn
        return socket.create_connection(("127.0.0.1", self.port), timeout=5)

    def prepare(self, unix=False, delay_ms=4000):
        self.configure(total=6000)
        if unix:
            self.config.write_text(self.config.read_text().replace(
                "unix_socket=none", f"unix_socket={self.root / 'http.sock'}"))
        self.mock(delay_ms=delay_ms)
        self.start()

    @staticmethod
    def send_metadata(conn):
        conn.sendall(b"GET /backend-api/wham/usage HTTP/1.1\r\n"
                     b"Host: localhost\r\nConnection: close\r\n\r\n")

    def wait_record(self, kind, timeout):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            entries = self.recorded(kind)
            if entries:
                return entries
            time.sleep(0.01)
        return self.recorded(kind)

    def check_cancelled(self):
        self.assertEqual(len(self.wait_record("aborted", 1)), 1,
                         "confirmed disconnect kept the upstream metadata request alive")
        self.mock()
        # The original worker and shared host must be reusable immediately.
        started = time.monotonic()
        status, _, body = self.metadata()
        self.assertEqual(status, 200, body.decode())
        self.assertLess(time.monotonic() - started, 1)
        self.assertEqual(len(self.recorded("host_started")), 1)
        self.assertEqual(len(self.recorded("metadata")), 2)

    def test_tcp_reset_before_response_head_cancels_only_its_request(self):
        self.prepare()
        with contextlib.closing(self.connection()) as conn:
            self.send_metadata(conn)
            self.assertEqual(len(self.wait_record("metadata", 3)), 1)
            # RST is a definitive abort. A normal FIN can also be a valid write half-close.
            linger = struct.pack("hh" if os.name == "nt" else "ii", 1, 0)
            conn.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, linger)
        self.check_cancelled()

    @unittest.skipUnless(hasattr(socket, "AF_UNIX") and os.name != "nt", "Unix sockets required")
    def test_unix_full_close_before_response_head_releases_the_worker(self):
        self.prepare(unix=True)
        with contextlib.closing(self.connection(unix=True)) as conn:
            self.send_metadata(conn)
            self.assertEqual(len(self.wait_record("metadata", 3)), 1)
        self.check_cancelled()

    def check_half_close(self, unix=False):
        self.prepare(unix=unix, delay_ms=100)
        with contextlib.closing(self.connection(unix=unix)) as conn:
            self.send_metadata(conn)
            conn.shutdown(socket.SHUT_WR)
            response = http.client.HTTPResponse(conn)
            response.begin()
            body = response.read()
            self.assertEqual(response.status, 200, body.decode())
            self.assertIsInstance(json.loads(body), dict)
        self.assertEqual(len(self.recorded("metadata")), 1)
        self.assertEqual(self.recorded("aborted"), [])

    def test_tcp_write_half_close_still_receives_the_response(self):
        self.check_half_close()

    @unittest.skipUnless(hasattr(socket, "AF_UNIX") and os.name != "nt", "Unix sockets required")
    def test_unix_write_half_close_still_receives_the_response(self):
        self.check_half_close(unix=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for arg in ("binary", "community-plugin", "pluginctl", "redact-plugin", "privacy-worker", "routing-plugin"):
        parser.add_argument("--" + arg, required=True)
    parser.add_argument("--bun", default=shutil.which("bun"))
    args = parser.parse_args()
    for key, value in vars(args).items():
        if not value:
            parser.error(f"missing {key}")
        setattr(HttpCancellationTests, key, Path(value).resolve())
    names = sorted(name for name in HttpCancellationTests.__dict__ if name.startswith("test_"))
    result = unittest.TextTestRunner(verbosity=2).run(
        unittest.TestSuite(HttpCancellationTests(name) for name in names))
    raise SystemExit(0 if result.wasSuccessful() else 1)


if __name__ == "__main__":
    main()
