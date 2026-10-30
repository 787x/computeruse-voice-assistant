> Current product contract: voice confirmation is bidirectional. Whole affirmative utterances approve, whole negative utterances reject; questions/quotes/conditions/ambiguity remain pending. The earlier cancellation-only/click-only conclusions below are historical and superseded by the final section.

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


## Initialization ownership and offline cleanup follow-up (2026-10-30)

Reviewed starting at PR HEAD `755eeb2397278eea894801838e411dd311759e76`. Both versions now use the same small `AgentInitialization` owner. It tracks pending consumers independently of any single turn, owns each toolkit immediately upon construction, revokes a job when its last consumer leaves, and publishes only a completed, still-valid runtime. Shared valid consumers can continue; a new consumer can start a replacement job after revocation. Identity checks prevent old `finally` callbacks from clearing newer loading state. Successful runtimes remain reusable until application shutdown. Cleanup failures are diagnosed and retained for a later cleanup retry.

Every asynchronous initialization boundary checks ownership before beginning another operation. Music synchronization/availability checks are awaited instead of detached; root credential synchronization receives the initialization signal, aborts CLI subprocesses and checks before subsequent writes. LX retains its separate availability behavior. Imports and already-started configuration/index work cannot all be interrupted; their results are checked before native startup, publication or subsequent effects. Already-started writes cannot be rolled back.

Shutdown revokes initialization before waiting for microphone/TTS teardown and closes both cached and in-construction toolkits. NativeBridge disposal now marks the bridge closed immediately, rejects startup/pending requests, kills its owned process without queueing a shutdown request behind warmup, and waits up to one second for exit. A failed close retains the process handle and can be retried; the existing five-second application hard-stop remains the final exit bound. This cannot guarantee process termination if the OS refuses termination, and such failure is logged rather than treated as a successful cleanup.

Online, explicit offline and network-error fallback paths now share a single `try/catch/finally`. Fallback exceptions are reported once through existing error UI, never recursively retried. Only the owning turn clears its confirmation, command timer and active state. Offline cancellation is checked after launcher initialization and launch; an operation already launched may complete, but cannot produce a late reply or continue the old turn. Search-key refresh also captures runtime ownership so an old refresh cannot update a new runtime.

### Executed and passed for both versions

| Suite | Assertions / result |
|---|---|
| `node tools/verify-initialization.mjs` | **19 scenarios each**. Actual shell initializer with injected module loaders; pre-aborted start, import/config/key/music cancellation before bridge creation, warmup/search/launcher cancellation and failures, later Agent construction failure, retry after failure, shared consumers, runtime reuse, old completion/finally versus a replacement job, exit races, cleanup retry. Asserts creation/disposal/live counts and runtime/loading identity. Root additionally passed 2 mocked credential-CLI cancellation scenarios (no subsequent write/command). |
| `node tools/verify-offline-cleanup.mjs` | **12 scenarios each**. Actual runCommand/runOfflineCommand functions with mocked launcher; explicit offline and network fallback each cover launcher/launch failures, success, cancellation during both awaits and late failure alongside a new turn. Asserts active turn, UI, confirmation close count, no subsequent launch and successful follow-up. |
| `node tools/verify-bridge-lifecycle.mjs --real` | **5 mock-child scenarios + 4 real PowerShell scenarios each**. Mock cases assert request/kill/live counts, startup/banner/pending-call disposal and retry after a failed kill. Real cases: cancellation while starting, close while starting, close after ready and actual shell shutdown-function cleanup while microphone shutdown is paused. Every real owned child was created once, disposed once, reported exit and its PID no longer existed (`process.kill(pid, 0)` returned ESRCH). |
| Existing confirmation / Agent / turn / permissions regression | Both `verify-confirmation`, `verify-agent-cancellation` (including loopback HTTP/SSE abort), `verify-turn-cancellation` passed; permissions remained **65/65** each. |
| Existing `node tools/verify-p1.mjs` | Both passed; mock streaming model and real read-only list_windows, with temporary config. |
| Existing Electron confirmation harness | Both passed all 9 checks on **Electron 43.4.0**; real production preload/renderer + extracted confirmation functions, isolated user-data, trusted Chromium input, no real tool execution. |
| Syntax / diff checks | Both affected shell/Agent/CUA modules and tests passed Node syntax checks; `git diff --check` passed. |

Run corresponding `lx/tools/...` scripts for the second version. The new Node suites do not read or write real user configuration and do not delete files or run dangerous commands. The `--real` suite starts only the native bridge and verifies its lifecycle, without invoking desktop action tools. It instruments the existing NativeBridge spawn dependency through CuaToolkit's constructor to record the exact owned child handles/PIDs. The shell shutdown function is genuine code executed with simulated microphone/app-exit objects and real toolkits; this is not a claim that a full Electron production exit was tested.

### Not run / remaining limits

No new full production Electron bootstrap/exit, packaged-build, real microphone/ASR, real model-provider or music-credential synchronization test was run. Electron 45.0.0 remains unverified; the available 43.4.0 executable was reused for the confirmation harness. Mock initialization and offline tests cannot establish real launcher/music behavior. Real native PowerShell disposal **was** tested separately as described above. There are no remaining failed suites from this follow-up; failure/kill-failure inputs in the regression suites are intentional cases with explicit cleanup assertions.


## Failed termination, observable close and on-demand music (2026-10-30)

Baseline: `a94fcfe`. NativeBridge now retains the current child until its matching exit event. Failed/throwing kill requests remain observable through `terminationFailure`; restart is refused while the prior child has not exited. Startup/request timeout, abort and disposal share this ownership rule. Repeated cleanup can retry; callbacks verify child identity, old streams are detached on exit and late old events cannot modify a replacement's requests or readiness. Disposed bridges cannot restart.

AgentInitialization.close now returns `{ok, remaining, errors}`, identifying unresolved resource types and reasons, after at most two attempts per resource. Successful resources are not disposed again; failed resources remain owned for later retries. Production shutdown logs this structured result, handles unexpected rejection and preserves its five-second hard-stop. A hard-stop remains an exit bound, not proof of resource cleanup.

Music is no longer a prerequisite for Agent creation. Root credentials are prepared on demand before the actual music tool, using a serialized runtime queue, fresh config, matching-hash cache and invocation cancellation signal. Concurrent music calls do not resync already-matching credentials; changed credentials cause the older preparation to reject instead of persisting its stale result or proceeding with music. No detached initialization task is introduced. LX leaves availability to its existing runLx path. Already-started saves/operations cannot be rolled back; the checks suppress subsequent work, not effects that already began.

Executed and passed, both root and lx:
- Initialization suite: 18 existing ownership scenarios (the obsolete eager-music cancellation scenario was replaced), hung-music nonblocking initialization, structured close transient/persistent failures with independent resources, and production shutdown partial failure/rejection/five-second hard-stop checks. Root additionally passed 2 credential-CLI cancellation and 3 on-demand music readiness/cancellation/cache/changed-config scenarios.
- Historical before/after test executes the actual initializer from `git show a94fcfe:<version>/apps/shell/main.mjs` against the same controllable hung music dependency. Baseline initialization remained blocked with no Agent constructed; current initialization completed without calling the unrelated dependency. This is a deterministic mock comparison, not a real-account latency benchmark.
- Bridge suite: 5 original + 5 new fault scenarios per version. Startup timeout with kill false/throw, request timeout/abort and failed disposal retain a live owned handle and refuse restart; retry cleans it. Old saved stdout/error/exit callbacks do not corrupt a replacement request; exited stream listeners are removed. Real PowerShell tests again passed all 4 scenarios per version with each owned PID absent afterward.
- Both offline cleanup, confirmation, Agent cancellation/HTTP-SSE abort, turn isolation, permissions (65 each) and P1 suites passed. Electron 43.4.0 confirmation harness again passed all 9 checks per version. Syntax/diff checks passed.

Development failures were corrected and rerun: the initial process-retention change exposed duplicate kill issuance during simultaneous startup cleanup/disposal (mock active count became -1); termination-in-progress suppression fixed it. The new music invocation fixture initially omitted lastUtterance, causing a fixture ReferenceError; the missing mock was added. No final suite remains failing. Expected injected termination failures are asserted negative cases, not passed cleanup claims.

Not run: Electron 45.0.0, full production Electron boot/exit, packaged builds, real microphone, real music accounts/credentials or real music latency. The shutdown hard-stop is simulated; native PowerShell PID cleanup is real and separate. Music cache/config tests use controlled mocks and do not modify actual user configuration. Persistent OS denial of termination remains reportable, retryable failure, not guaranteed physical cleanup.


## Spawn-close and credential consistency follow-up (2026-10-30)

Baseline `f683068`. Both NativeBridge versions now converge matching exit/close events through one idempotent terminal callback. Error alone never proves termination; error followed by close without exit releases the failed spawn object and allows retry. Dispose waits for exit or close; old callbacks remain identity-guarded. Retained-live-handle behavior for failed kill is unchanged.

Root settings-save and Agent music calls now share one application-wide CredentialSync queue. Settings save enqueues synchronization without waiting for CLI latency; ordinary Agent initialization still does not synchronize music. Each queue job loads current credentials, supports cancellation and terminal shutdown, and retries newer versions when a conditional hash save discovers a stale snapshot. HUD dismissal invalidates pending generations while allowing later valid jobs. The queue does not start a newer CLI mutation until the older operation has settled, so a delayed uninterruptible old write cannot finish after the newer CLI version has completed.

All root saveConfig operations are serialized, including the read/merge/write/rename sequence. Credential hash publication supplies expected credentials checked **inside** that serialized operation; this prevents old hash writes overwriting newer settings. Store access from main remains lazy, preserving the existing environment-dependent config directory setup. CLI mutation of a different version first clears the older success hash: this also fixes B→partial A→B cancellation, where a retained B hash would otherwise skip repair of the actual CLI state. Successful cached versions retain the skip optimization.

syncCredentialsAsync throws a distinguishable sanitized field-failure error on nonzero set, with no success hash returned. A failed get can still be followed by a successful set. Private-key contents are not inferred from masked/prefix CLI output; only prior successful hash caching skips the private-key write. Partial success leaves the hash invalid and permits retry. The synchronous legacy helper has no application call sites; runtime settings/Agent writers both use the coordinator. No网易云 code was added to LX.

Executed and passed:
- Both bridge lifecycle suites: prior 10 mock cases + 4 new event/spawn cases, covering synchronous spawn throw, error→close without exit, error→exit→close and exit→close, retry and old events. Both also passed the prior 4 real PowerShell cases **and a real missing-executable ENOENT→close→successful PowerShell retry→PID cleanup case**. Native PIDs were checked, not inferred from suite exit codes.
- Root `node tools/verify-credential-sync.mjs`: 9 concurrency/failure scenarios plus serialized-store CAS, queued shutdown and HUD-cancel/new-generation checks. Uses real store in a temporary directory and the actual sync function with a mock CLI. A/B saves, cancellations and B/A/B history assert both final CLI fields and the successful hash for B. Both required-field failures leave no success hash and retry succeeds; failed reads with successful writes are accepted; same-version concurrent requests use the cache. Settings/Agent ordering uses the same coordinator API as production; actual settings-window UI was not automated.
- Both initialization, offline cleanup, confirmation, Agent cancellation/SSE abort, turn-isolation, permissions (65 each) and P1 regressions passed. Both Electron 43.4.0 confirmation harnesses passed 9 checks. Syntax and diff checks passed.

Development failures, then fixed: newly extracted shutdown tests initially lacked the new music owner mock and threw ReferenceError; fixtures were updated. The initial concurrency test exposed a stale-success-hash scenario where the simulated CLI was not restored after an intervening version; invalidating the old hash before mutation fixed this, and explicit B/A/B cases now cover it. Final suites have no remaining failures.

Unverified: real网易云 accounts/private keys, real ncm-cli synchronization and account login retention, full Electron production startup/exit, packaged builds, real microphone and Electron 45.0.0. CLI/account conclusions above are controlled simulations. Existing external processes or writers outside this application are not coordinated by its in-process queue. Writes already started cannot be rolled back; cancellation prevents subsequent operations. OS-level termination denial remains a retained, observable cleanup failure.


## Product compatibility correction: bidirectional voice confirmation (2026-10-30)

Baseline checked: origin/main `06f097d` and PR HEAD `655ffdd`. This section supersedes earlier claims that speech can never approve. No history was rewritten and no force push or new PR was used.

Restored both versions: complete affirmative phrases (可以、确认、确定、同意、允许、好的、好、是的、对、执行、继续 and limited direct variants), explicit voice refusal, original default-on voiceConfirmEnabled meaning and persisted field, setting/tray label 语音确认, HUD voice-or-button instructions, and approval through ASR → finishCommand → settleConfirm → PermissionGate. Default confirmation timeout is still 180000 ms. The UI remains one utterance/one decision, without a mandatory click or second confirmation. Mouse/15-minute/deny buttons remain protected; trusted Enter works and focused buttons retain native keyboard activation. Escape keeps main's actual behavior of dismissing the assistant, now labeled 关闭 accurately.

Parser uses complete expressions with bounded normalization and no affirmative substring search. 不可以、不同意、不确认、不允许、不执行 and explicit cancellation refuse. 可以吗、可不可以、不是不可以、不要取消、我说的是可以、我说的是不可以, quotes, conditions and unrecognized text remain undecided. The quoted negative is conservatively undecided and cannot approve.

Voice settlement requires a private main-process Symbol supplied only by the local ASR path, matching current request ID and ASR generation, and an enabled setting. PermissionGate accepts voice or click from its injected confirmation callback. The page has neither the ASR authority nor a public approval API. IPC still validates window/frame/payload/ID and builds click answers itself. A voice label or a boolean trusted flag does not grant ASR authority.

Confirmation uses endpoint final directly, or the actual ASR flush result. An empty flush cannot fall back to lastCommandText/partial. Partial 可以 followed by final 不可以 never authorizes early. Invalid/ambiguous answers preserve the card and original deadline. Cancelling/replacing/timing out the request invalidates late voice results. Continuous/non-confirmation command fallback behavior remains intact.

### Scope and convenience audit against main

| Classification | Changes / disposition |
|---|---|
| Required original bug fix | Whole-expression affirmative/negative parsing and accurate ambiguity tests. Voice approval is preserved. |
| Compatible protections with regression value | Request IDs, ASR generations, switch reset, timeout default deny, private ASR source, protected preload clicks/keyboard, per-turn cancellation and late-result UI isolation. Retained without extra user steps. Main already described dismiss as terminating work; the abort plumbing implements that intent. |
| Independent technical fixes | Agent initialization owner, NativeBridge exit/close/retry, offline error cleanup, credential coordination and serialized saves. Kept in separate modules/test suites in existing commits; not necessary for the text parser and not expanded in this correction. Removing this intertwined work would be a substantial rollback; retained for review rather than rewriting/purging prior commits. Future separation can be done by selecting the isolated files/commits, but no new PR or branch was created here. |
| Unnecessary product regressions | Removed click-only and cancellation-only policy, restored default-on bidirectional voice and UI wording, removed speech partial fallback while retaining real final/flush, aligned keyboard hint/behavior. Eager music blocking was already removed; ordinary instructions do not await unrelated music. |

No permission tier, whitelist, unattended behavior, follow-up window duration, speech synthesis defaults, music implementation or launcher product setting was changed in this correction. Those module assertions are a source/diff audit, not a full real-device convenience test. Independent technical fixes remain visible in the cumulative PR and are explicitly not claimed as required for the original bug.

### Executed verification

Both versions:
- `verify-permissions.mjs`: **86 cases passed**, including exact affirmative/negative/undecided assertions, real parseConfirmText→PermissionGate voice approval, original policy/deny/pause/grant/unattended cases and rejection of unknown/test channels.
- `verify-confirmation.mjs`: actual production confirmation/audio functions and real parser, controlled ASR, real PermissionGate. 可以/确认/同意/好的 approve once without a click; negatives reject; partial-positive→negative-final, empty flush, valid flush, request replacement, stale ID/generation, switch off/on, forged internal voice claims, click/15-minute/repeat/timeout/abort all passed. No parser-return stub is used for these decisions.
- Electron **43.4.0** production preload/renderer harness: **11 checks passed**, now including real trusted Chromium Enter and rejected synthetic Enter, alongside protected clicks, no page approval API, temporary grant, rejection, timeout and abort. This tests real Electron UI/IPC pieces, not a real microphone.
- Existing initialization, Agent cancellation with local SSE abort, turn isolation, offline cleanup and P1 passed. Root credential synchronization suite passed. Node syntax and diff checks passed. NativeBridge suites (mock events plus real owned PowerShell PID scenarios) were also rerun as regression checks.

Earlier tests that assumed all speech approval was forbidden were corrected; refusal, ambiguity, request-isolation and fake-source tests remain. Historical cancellation-only reports are not the current acceptance criteria. No final failing suite remains.

Not verified: real microphone/ASR recognition quality, full production Electron session/continuous dialogue, packaged app, Electron 45.0.0, real music accounts/CLI login. ASR decisions in the Node suite are controlled finals/flushes, not microphone recordings. Broader convenience conclusions are limited to code comparison and the listed regressions.


## Natural phrasing and HUD cancellation ownership (2026-10-30)

Reviewed baseline `afc01b3`. This correction changes only two behaviors:
- Both parsers retain whole-expression matching and add 我同意、我确认、可以的、好啊 as affirmative; 不对、错了、停、不用、别 as refusal. Existing direct affirmative/negative expressions remain. No substring approval, model call or extra confirmation step was added.
- Root dismiss no longer invokes coordinator-wide cancelPending. It still calls cancelActiveTurn, whose AbortController is passed to Agent tools and voice-owned credential sync. Independent settings sync has no turn signal and survives HUD closure. Application shutdown still calls CredentialSync.close to cancel all managed work with a bounded result. The coordinator architecture and LX Music implementation are unchanged. Earlier historical claims that HUD invalidates the entire credential queue are superseded by this owner-scoped behavior.

Executed and passed:
- Both verify-permissions suites: **101 cases each**, including all requested natural phrases and undecided question/quotation/conditional/double-negative cases.
- Both verify-confirmation suites: real parser, actual confirmation/audio functions and real PermissionGate with controlled ASR. Added phrases approve/refuse directly. Ambiguous speech preserves the card and existing deadline. Existing partial-positive→negative-final, valid/empty flush, off/on, request ID/generation, timeout/cancellation, click/temporary-grant and forged-source assertions passed.
- Root verify-credential-sync: prior 9 concurrency/failure scenarios and CAS/shutdown tests passed; added **3 production dismiss/cancelActiveTurn scenarios** with actual coordinator, real temporary config and mock CLI: settings sync executing; voice A executing with saved settings B queued; settings executing with voice + independent settings queued. Each asserts the turn signal is aborted while independent sync completes, final config/private key/hash and simulated CLI correspond to B, cancelled A does not start its next CLI write, and a subsequent valid request succeeds. Also passed a bounded-close test reporting an uninterruptible mock as remaining until released. The explicit coordinator-wide cancelPending test is retained and correctly labeled; HUD does not call that operation.
- Both initialization, Agent cancellation/local SSE, turn isolation, offline cleanup and P1 regressions passed. Both Electron **43.4.0** production preload/renderer confirmation harnesses passed **11 checks**, including mouse, temporary grant, Enter and synthetic-event rejection. Relevant syntax and diff checks passed.

No final failing suite. All CLI credentials/config in the new tests are synthetic and stored in a temporary directory. Settings-save responsiveness is preserved by the unchanged non-awaiting enqueue path (source inspection), not measured through a real settings UI. Real microphone/ASR recognition, real music accounts/CLI, full production Electron session, packaged builds and Electron 45.0.0 remain unverified. No NativeBridge/login/clear behavior was changed or broadened in this correction.
