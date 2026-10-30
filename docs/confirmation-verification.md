# Confirmation integration verification (2026-10-30)

Runtime: existing Electron 43.4.0 from the local megawatt-flash-charge-simulator-ui project, Windows x64. The repository lockfile specifies Electron 45.0.0; that version remains unverified. No dependency declarations or lockfiles were changed.

Tests ran in a temporary source copy with separate `.config` and Chromium user-data directories. Background orphan cleanup, legacy autostart cleanup, player/schedule watchers, prewarming and resource monitoring were disabled only in that temporary copy. The test window was repositioned and raised to avoid desktop occlusion. The confirmation implementation, preload and button handlers were unchanged. No clipboard read or other tool was executed: `--gate-test` only requests permission verdicts.

`--gate-test click`, `once` and `deny` now wait for an actual confirmation-card click instead of calling the settlement function directly. Windows computer-use input clicked the respective buttons. `timeout` used `CUA_VA_CONFIRM_TIMEOUT=2000` without a response. Run these modes on a similarly isolated copy, with standard permissions and wake disabled, to reproduce.

| Version | Mode | Result | Confirmation channel | Approved |
|---|---|---|---|---|
| root | click | PASS | click | True |
| root | once | PASS | click | True |
| root | deny | PASS | click | False |
| root | timeout | PASS | timeout | False |
| lx | once | PASS | click | True |
| lx | click | PASS | click | True |
| lx | deny | PASS | click | False |
| lx | timeout | PASS | timeout | False |

Both `once` runs also returned `allow_once` without showing another card for the next check. Each run verified normal policy allowance and unknown-tool rejection.

Pure logic regression tests were rerun: both `verify-permissions.mjs` suites passed 41 cases each, both `verify-confirmation.mjs` lifecycle suites passed, both main-process syntax checks and `git diff --check` passed.

Not verified: Electron 45.0.0, packaged builds, real microphone/ASR, tray interaction and production background initialization. Wake, disabled voice, stale/repeated events and erroneous affirmative parsing are covered by the mocked lifecycle regression suite, not by microphone end-to-end tests.
