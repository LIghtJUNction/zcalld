# Maintenance and dependency propagation

## User-approved rule: complete downstream propagation

When a Project Z library changes, its owner is responsible for the complete reverse-dependency chain, including libraries, applications, services and installation packages. Publishing the library or sending a notification alone does not finish the change.

1. Enumerate direct and indirect consumers. Use the Project Z graph: `python3 scripts/project_z.py impact --repositories <repository-or-package>`. Register new consumers before relying on the graph.
2. Coordinate with each existing owner before editing an actively maintained checkout. Do not start a duplicate owner or overwrite another owner's in-progress work.
3. Update package versions where appropriate, exact source revisions, git submodule links, Cargo patches and lockfiles.
4. Validate each affected consumer at its own boundary: build/API tests for libraries, real platform behavior for applications, and package/service installation for distributed components.
5. Commit and push each coherent change promptly. Collect the actual commit and test evidence from delegated owners.
6. Close the propagation record only when every affected consumer is updated and verified. If blocked, record the concrete blocker, current safe state and named owner who will continue. A notification is a handoff, not completion.

This rule was explicitly requested on 2026-11-20 (Asia/Taipei). Existing owners should collaborate directly; no new model or subagent is needed solely to apply the rule.

## Current project boundaries

| Component | Authority |
| --- | --- |
| zcalld core, packages, migration/cutover | zcalld maintainer |
| zcalld-compat optional privacy/community host | compatibility maintainer |
| zcall-gui application and native packages | GUI maintainer |
| zgpui package / zgpui repository | zgpui maintainer, with the Project Z coordinator owning the agreed single registry publication |
| Other Project Z downstream repositories | their existing owners; updates and evidence must return to the originating library owner |

The root repository pins zcall, zconv, zplugin, zsys, compatibility and GUI sources. The GUI pins zgpui through vendor/zgpui. Both levels are part of a zgpui propagation; updating only the GUI's inner gitlink is insufficient.

## Release records

Each release record should include the old/new source revisions, reverse-dependency inventory, owner acknowledgements, downstream commits, native-platform evidence, package results and any remaining blockers. Keep user configurations, accounts, credential fingerprints and device transaction journals outside public records.

Prefer RTK for routine local Git/Cargo/read summaries when available. Release scripts and installed programs must not gain an RTK runtime dependency.
