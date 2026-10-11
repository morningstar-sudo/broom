# Contributing to Broom

Thanks for helping! Bug reports, fixes, documentation and testing on real hardware are all welcome. Please read the
[Code of Conduct](CODE_OF_CONDUCT.md) first. Security problems: **don't open an issue** — see [SECURITY.md](SECURITY.md).

Vietnamese or English, both are fine in issues and pull requests.

## Reporting a bug

Use the **Bug report** template. The most useful things to attach:

- the release you run (e.g. `v0.5.5-deb9fbe`; the version alone is under "Broom" at the top left of the web page)
  and the server OS;
- the server log: `journalctl -u bootrom-mgmt --since "1 hour ago"` (and `journalctl -u broom-iscsid` for iSCSI);
- for a Windows client: a photo of the stage screen, or `broom\stage.log` from the BROOMWIN partition; for the
  games disk / watchdog: `C:\Windows\Temp\broom-games.log`;
- what the machine is (real hardware or VM, NIC, disk) and the image's OS.

**Redact before posting:** product keys, passwords, the admin setup token, and if you prefer MAC/IP addresses and
hostnames. Never attach a golden image, `bootrom.db` or an export.

## Suggesting a feature

Use the **Feature request** template and describe the situation in the room (how many machines, what goes wrong
today) before the solution — it is easier to find the simplest fix that way.

## Development setup

See **Build** in the [README](README.md): `./build.sh` on Linux / WSL (`build.cmd` on Windows) builds iPXE when
needed, the static musl binary and runs the unit tests. Requirements: Rust stable via rustup (toolchain pinned in
`mgmt/rust-toolchain.toml`), `musl-tools`, `git`, `gcc`, `make`, `perl`, `liblzma-dev`. Never build with sudo.

Quick loop while working on the server (in `mgmt/`):

```bash
cargo test --release            # unit tests (no root needed)
cargo test --release iscsid     # one module
./build.sh --live               # also the root-only NTFS / ping / LVM tests
```

Run a test server without touching the real one: `BOOTROM_HOME=/tmp/broom-dev ./target/release/bootrom-mgmt
--skip-preflight --port 8095` (no DHCP / TFTP / iSCSI unless run as root).

## Where things live

| Area | Files |
|---|---|
| Web server, API, pages | `mgmt/src/*.rs`, `mgmt/static/` |
| iSCSI target daemon | `mgmt/src/iscsid/` |
| Windows publish, stage, prep | `mgmt/src/winstage/`, `mgmt/scripts/stage.sh`, `mgmt/scripts/*.ps1` |
| Linux goldens | `mgmt/src/overlay.rs`, `mgmt/scripts/linux-*.sh`, `mgmt/scripts/prep-linux.sh` |
| Database | `mgmt/src/db/` |

## Rules that are easy to miss

- **New file served or handed out** (a `static/page-*.html`, a script under `scripts/`): add it to the list in
  `mgmt/src/assets.rs`. Release builds load assets from `broom-assets.zip`, packed from that list — a missing entry
  makes the server panic at start.
- **Windows scripts (`*.ps1`) are ASCII only** (written with `-Encoding ascii`; a test checks it).
- **Stage changes** (`scripts/stage.sh`, `scripts/stage-hook.sh`, the tool list in `winstage/stage.rs`) reach clients
  only through a new stage bundle — CI builds it; for a local test use `bootrom-mgmt build-stage` (README).
  Stage tools copied into the initramfs must stay in `STAGE_TOOLS`: the initramfs busybox versions can be far
  slower or lack options.
- **Database schema**: change `SCHEMA` in `db/sqlite.rs`, bump `SCHEMA_VERSION` and add an `if v < N` step to
  `migrate()`; existing DBs must open without manual steps.
- **Shell scripts that run in the initramfs** must work with busybox `sh`; check with `sh -n` and, where there is one,
  the test in `winstage/stage.rs` that runs the script's section with mocked commands.
- **Comments** explain *why* and match the style of the surrounding code; no internal ticket or step codes in them.
- Anything that runs on clients (stage, `.ps1`, initramfs hooks) should be tried on a real machine or a VM before the
  PR is marked ready — say what you tested in the PR.

## Pull requests

1. Fork, branch from `main`, keep the change focused (one fix or feature per PR).
2. `./build.sh` passes (build + tests). Add a test for new logic where it fits — the existing tests show the style
   (pure functions tested directly; shell sections run with mocked commands).
3. Update the README when behaviour, settings or requirements change.
4. Commit messages follow [Conventional Commits](https://www.conventionalcommits.org): `fix: …`, `feat: …`,
   `chore: …`, `docs: …` — subject in the imperative, the *why* in the body when it isn't obvious.
5. Fill in the PR template. CI must be green before review.

By contributing you agree that your contribution is licensed under the project's
[Apache License 2.0](LICENSE).
