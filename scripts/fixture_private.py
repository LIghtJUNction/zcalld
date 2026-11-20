"""Create fixture credentials through the same native permission path as users."""
import json
import os
from pathlib import Path
import subprocess


def create_privacy_key(root, *, binary, redact_plugin, privacy_worker):
    """Return a random key protected by POSIX modes or a private Windows DACL.

    Keep the bootstrap config in its own directory so the test gateway cannot
    discover its binding and accidentally load privacy twice. This command does
    not start a gateway, a privacy worker, or a provider host.
    """
    directory = Path(root) / "privacy-key-fixture"
    directory.mkdir(mode=0o700)
    config = directory / "config.conf"
    config.write_bytes(b"[server]\nlisten=127.0.0.1:1\nunix_socket=none\n")
    environment = {key: value for key, value in os.environ.items()
                   if not key.startswith(("ZCALLD_", "CODEX_", "XDG_"))}
    environment.update(HOME=str(directory), USERPROFILE=str(directory),
                       XDG_CONFIG_HOME=str(directory / ".config"),
                       APPDATA=str(directory / "AppData/Roaming"),
                       ZCALLD_REDACT_PLUGIN=str(redact_plugin),
                       ZCALLD_PRIVACY_WORKER=str(privacy_worker))
    result = subprocess.run(
        [str(binary), "privacy", "set", "secrets", "--config", str(config), "--json"],
        env=environment, capture_output=True, text=True, encoding="utf-8", timeout=25,
        check=False,
    )
    if result.returncode != 0:
        raise AssertionError("native fixture key creation failed: " + result.stdout + result.stderr)
    if not json.loads(result.stdout).get("configured"):
        raise AssertionError("native fixture privacy binding was not created")
    binding = json.loads((directory / "privacy/plugin.json").read_text(encoding="utf-8"))
    key = Path(binding["key"])
    if not key.is_file() or key.stat().st_size != 32:
        raise AssertionError("native fixture key is missing or has an invalid size")
    if os.name == "posix" and key.stat().st_mode & 0o077:
        raise AssertionError("native fixture key is not private")
    return key
