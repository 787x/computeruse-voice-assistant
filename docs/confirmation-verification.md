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


## Review follow-up verification (2026-10-30)

The earlier eight interactive runs above describe HEAD `181b2dd` before the review follow-up. The results below apply to the strengthened isolated-preload and turn-cancellation implementation.

### Changes

- Removed the public `hud.answerConfirm` API. Approval and temporary approval are sent only by isolated-preload button listeners after checking `event.isTrusted`; the private request ID is not writable through the page API. Main-process sender, main-frame, payload and request-ID checks remain in place.
- Trusted DOM events are not human identity authentication. OS input automation and privileged Chromium/Electron input APIs can generate trusted input; this boundary blocks ordinary renderer-page JavaScript from directly approving.
- Each shell turn owns an AbortController and scoped event callback. Dismiss/quit/superseding commands abort it, settle pending confirmation, clear recognition/timers and stop speech. Cancellation is checked after model/permission/tool waits and before subsequent tools, including CUA recovery. Late callbacks/results cannot restore old UI or close a new turn's confirmation. A tool that already started and does not support interruption may still finish; completed effects are not undone.
- Complete cancellation phrases now include common forms such as 不行、别执行了、不要执行、取消吧、算了吧、停止操作. Questions, quoted phrases, conditional phrases and 不要取消 remain undecided. Speech never approves.
- Settings and tray use one voice-setting function, which resets ASR text, command timers and the voice generation. Re-enable arms cancellation listening only with microphone/ASR available. Neither toggle replaces the confirmation ID nor changes its timeout.

### Executed and passed

Both root and `lx/`:

| Command / suite | Result | Environment |
|---|---|---|
| `node tools/verify-permissions.mjs` | 65 cases passed | Real PermissionGate/parser, no tool execution |
| `node tools/verify-confirmation.mjs` | Passed | Actual shell confirmation functions with simulated ASR/window/timers; invalid source/frame/ID/payload, duplicate/grant, disabled voice and off/on stale generations, unchanged deadline, missing microphone/ASR, aborted Promise |
| `node tools/verify-agent-cancellation.mjs` | 7 cancellation/control scenarios + HTTP/SSE abort passed | Real Agent; fake tools/LLM for races, real LLMClient and loopback server for streaming abort |
| `node tools/verify-turn-cancellation.mjs` | 4 async phases + late follow-up + 2 recovery phases passed | Actual shell functions in VM; refresh/init/model/late error, dismiss, new-turn isolation, recovery cancellation |
| `node tools/verify-p1.mjs` | `verdict.pass: true` | Existing mock-model SSE integration, real read-only CUA `list_windows`, isolated temporary config |
| `electron tools/verify-electron-confirmation.mjs` | 9 checks passed | Existing local Electron **43.4.0**, real production preload + renderer and actual confirmation functions; separate temporary user-data, no production bootstrap or real tools |
| Syntax checks and `git diff --check` | Passed | Both main/preload/renderer/Agent implementations |

For `lx/`, run the same commands in that directory (or prefix Node script paths with `lx/`). For Electron, use an explicit installed executable; e.g. `electron.exe D:/Projects/computeruse-voice-assistant/tools/verify-electron-confirmation.mjs`, then the corresponding `lx/tools/` entry. Each harness writes its stage/result JSON to `<version>/.tmp/electron-confirmation-results.log`, exits nonzero on failure and has a watchdog. It isolates user-data and never calls a real tool. The Electron suite asserts the public approval API is absent, `.click()`/synthetic MouseEvents fail to settle, page callback mutation cannot replace the preload ID, trusted Chromium input can approve, temporary approval is single-use, and deny/timeout/abort settle safely. Trusted inputs in this automated suite use `webContents.sendInputEvent`; they are not evidence of human input.

### Failures during development, then corrected

The old confirmation VM test initially failed because its function-extraction marker no longer matched the new signature; it was updated and both suites passed. The first Electron harness hung/timed out during startup because asynchronous top-level initialization prevented the test entry from driving Electron readiness correctly. The harness was changed to synchronous pre-ready setup and an asynchronous entry function; both final Electron runs passed. These were test-harness failures, not passing results or unverified claims about production behavior. No final executed suite remains failing.

### Not run / limits

Electron **45.0.0** (the lockfile version) is still unavailable locally; the prior official binary download did not complete. The final Electron tests used 43.4.0, not 45.0.0. No packaged-build, real microphone/ASR, tray/settings UI click, real model service or full production-bootstrap test was run for this follow-up. Dynamic voice switching was tested with simulated ASR/listening state and both UI entry points were checked to call the common setter. The actual Electron fixture loads production preload/renderer and confirmation code, but not the full app startup. No deletion or dangerous command was tested; real config was not modified.
