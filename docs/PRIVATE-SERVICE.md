# First service setup for a private Linux installation

A full native archive supplies the gateway and its plugins. Initializing the user's active configuration and registering a service are explicit steps after the installer commits the verified private installation.

```sh
/absolute/private/runtime/bin/zcalld status --json
/absolute/private/runtime/bin/zcalld init --config /absolute/user/config.conf --json
/absolute/private/runtime/bin/zcalld service install --config /absolute/user/config.conf --json
/absolute/private/runtime/bin/zcalld service start --config /absolute/user/config.conf --json
```

Run `init` only when status reports both `configured=false` and `configuration_exists=false`. `configuration_path` gives the absolute path that was inspected. A dangling symlink, unreadable node, directory, or other inspection error counts as an existing choice. The CLI's exclusive create also rejects a file that appears after the status check.

`service install` is Linux-only and prepares `$XDG_CONFIG_HOME/systemd/user/zcalld.service`, or the corresponding `$HOME/.config` path. It verifies the complete installer-owned cohort while holding its runtime lease. The unit uses the actual installed executable and the explicitly selected, already valid configuration.

Registration does not start or enable the service. It returns `created=true` for a new unit and `created=false` only when the existing unit exactly matches this installation and configuration. It preserves and rejects different units, masks, service overrides, and packaged units in the search path. A GUI bootstrapping a newly installed prefix must complete this registration check before offering to start it; a same-named service alone does not establish ownership.

`service_install_supported` is a lightweight platform and installation hint. `service_installed` is true or false when the unit's presence can be determined, and null when the manager cannot be queried. Full installation and unit ownership are checked by the explicit `service install` action.

The unit follows systemd's [command-line syntax](https://www.freedesktop.org/software/systemd/man/latest/systemd.service.html#Command%20Lines) and [specifier rules](https://www.freedesktop.org/software/systemd/man/latest/systemd.unit.html#Specifiers). It disables environment expansion for `ExecStart`, quotes arguments, and escapes literal percent signs. Systemd rejects executable paths containing quotes or backslashes, so setup reports an error for those paths. Configuration arguments can contain these characters and are escaped.

The focused test uses real CLI and installer binaries with pinned local fixture archives, fake systemd commands, and temporary homes. It covers absent versus invalid configuration, exact ownership, no-clobber publication, idempotency, unsafe directories, modified bundles, and the absence of start or enable calls. Offline systemd verification checks the generated unit without executing the gateway. No user service, credential, or provider is involved.
