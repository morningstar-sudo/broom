## What and why

<!-- What this changes, and the problem it solves. Link the issue: "Fixes #123". -->

## How it was tested

<!-- Delete what doesn't apply, add what you did. -->

- [ ] `./build.sh` passes (build + unit tests)
- [ ] New logic has a test (pure function, or a stage section run with mocked commands)
- [ ] Tried on a real machine / VM — describe: <!-- e.g. Windows 11 client on VMware, golden download + reboot -->

## Checklist

- [ ] One focused change; commit messages follow Conventional Commits (`fix:`, `feat:`, `docs:` …)
- [ ] New served / handed-out file added to `mgmt/src/assets.rs`
- [ ] Windows scripts (`*.ps1`) are ASCII only
- [ ] DB schema change: `SCHEMA_VERSION` bumped + `migrate()` step
- [ ] Stage change (`stage.sh`, `stage-hook.sh`, `STAGE_TOOLS`): noted — clients get it with the next stage bundle
- [ ] README updated if behaviour, settings or requirements changed
- [ ] No keys, passwords, tokens or private addresses in the diff, logs or screenshots
