#!/usr/bin/env python3
"""Run portable native Rust-to-Go/Bun compatibility acceptance without real accounts."""
import argparse
import os
from pathlib import Path
import subprocess
import sys

root = Path(__file__).resolve().parent.parent
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--binary", type=Path)
parser.add_argument("--pluginctl", type=Path)
parser.add_argument("--privacy-worker", type=Path)
args = parser.parse_args()
executable_suffix = ".exe" if os.name == "nt" else ""
library_prefix = "" if os.name == "nt" else "lib"
library_suffix = ".dll" if os.name == "nt" else ".dylib" if sys.platform == "darwin" else ".so"
binary = (args.binary or Path(os.environ.get(
    "ZCALLD_TEST_BINARY", str(root / "target/release" / ("zcalld" + executable_suffix))
))).resolve(strict=True)
pluginctl = (args.pluginctl or Path(os.environ.get(
    "ZCALLD_TEST_PLUGINCTL", str(root / "_release/native-compat" / ("zcalld-pluginctl" + executable_suffix))
))).resolve(strict=True)
privacy_worker = (args.privacy_worker or Path(os.environ.get(
    "ZCALLD_TEST_PRIVACY_WORKER", str(root / "_release/native-compat" / ("zcalld-privacy" + executable_suffix))
))).resolve(strict=True)
redact = (binary.parent / (library_prefix + "zcalld_redact" + library_suffix)).resolve(strict=True)
routing = (binary.parent / (library_prefix + "zcalld_plugin_routing" + library_suffix)).resolve(strict=True)
community = (binary.parent / (library_prefix + "zcalld_community" + library_suffix)).resolve(strict=True)
common = ["--binary", str(binary), "--redact-plugin", str(redact),
          "--privacy-worker", str(privacy_worker)]
subprocess.run([sys.executable, str(root / "scripts/test-privacy-init.py"), *common],
               cwd=root, check=True)
subprocess.run([sys.executable, str(root / "scripts/test-transport.py"), *common,
                "TransportTests.test_compressed_requests_are_masked_before_provider_egress"],
               cwd=root, check=True)
subprocess.run([sys.executable, str(root / "scripts/test-routing-advanced.py"), *common,
                "--pluginctl", str(pluginctl), "--routing-plugin", str(routing)],
               cwd=root, check=True)
subprocess.run([sys.executable, str(root / "scripts/test-mcp.py"),
                "--binary", str(binary), "--pluginctl", str(pluginctl),
                "--community-plugin", str(community)], cwd=root, check=True)
subprocess.run([sys.executable, str(root / "scripts/test-builtin-fetch.py"), *common,
                "--pluginctl", str(pluginctl), "--community-plugin", str(community)],
               cwd=root, check=True)
print("Native compatibility accepted: privacy, routing, MCP and generated builtin provider fetch over real Rust/Go/Bun IPC")
