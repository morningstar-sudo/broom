# Rules — How we work

## After EVERY completed phase (mandatory)
1. **Review the code** just written: right module, no bloat, follows `plan.md`.
2. **Write detailed test steps** for the current phase into `progress.md` — exact commands,
   inputs, **expected vs actual result**. Enough for someone else to rerun them exactly.
3. Only mark a phase **done** in `progress.md` when the tests **really pass** (with
   evidence: log/output/screenshot), no guessing. Fail → record the error clearly, **don't move to the next phase**.
4. Update `plan.md` / `rule.md` if the design changes.

## General principles
- 1 module = 1 file, no dumping. Split clearly along M1–M8.
- No new dependency/intermediate layer until needed (nginx dropped, plain NFS for Linux).
- The golden image is shared **read-only**; runtime writes: a write layer on the **local SSD**, reset every
  boot (Linux: overlayroot; Windows: child VHDX). PXE first in BootOrder so every boot goes through the reset.
- Preflight must pass before the app serves (missing packages → exit ≠ 0, reported clearly).
- **Current scope:** Linux diskless (iSCSI + SSD writeback/cache) + **Windows design B** (native VHDX
  boot on the client SSD, child reset every boot — see plan.md section "Phase W", docs/phase-w-windows.md).

## Phase status
`todo` → `doing` → `done`. Only one phase `doing` at a time.
