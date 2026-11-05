# Gemini Web end-to-end review

Date: 2026-11-04. Scope: the current working tree in `D:/Dev/ccat`, the installed release, and the original Argon source as reference. The review used Opus for source analysis and an independent Opus runtime workflow. This was a review, not a repair or a completed native migration.

## Verdict

**The implementation is still a wrapper, not the native integration the user requested.**

The current path is:

```text
Claude Code
  -> ccat's routing proxy
  -> configurable localhost Bridge URL
  -> separately launched, copied gemini-api.exe
  -> Gemini Web

Google login
  -> ccat starts PATH python
  -> extracted login.py imports external Playwright
  -> installed Chrome
  -> DPAPI credential file
```

Rust implements settings, routing, some account-file operations, and a DPAPI envelope. The actual Gemini authentication/session initialization, internal model routing, generation, prompt-based tool bridge, and response conversion remain in the Go executable. Moving the executable into AppData and embedding the Python script's text do not migrate those implementations into ccat.

The Bridge URL field is therefore not just leftover wording: it reflects the current architecture. Hiding that field alone would not satisfy the request. Earlier claims of a fully native, feature-complete migration, reliable automatic failover, real usage limits, or measured 60 FPS were not supported by the implementation and verification.

## Confirmed source findings

The following are the ten primary findings returned by the Opus source review, ordered with credential safety and the reported request failures first. Architectural and feature-parity gaps are recorded separately below. P1 means high-priority correctness/security work; P2 means a material but lower-priority defect.

### 1. P1 — Claude credentials can be forwarded to the Gemini bridge

Evidence: `src/proxy/mod.rs:541`, `src/proxy/http.rs:277-292`, `src/main.rs:1986-1992`.

The Gemini branch calls `forward(..., key=None, ...)`. The shared forwarder strips `authorization` and `x-api-key` only when `key.is_some()`. Claude Code's own authorization remains available for official-Claude tiers, with ccat's routing token carried separately. Consequently, a Gemini request in a session with a Claude login can send the real Claude credential to the bridge. A user-configured remote URL, or a different process occupying the local port, can receive it. This is a source-confirmed exposure path, not evidence that credentials have actually been captured.

Required correction: provider credential isolation at the request boundary; Gemini requests must never carry Claude or local gateway credentials.

### 2. P1 — Error handling prevents the intended recovery branch from running

Evidence: `src/proxy/mod.rs:541-559`, `src/proxy/mod.rs:572-594`, `src/proxy/http.rs:295-311`.

The new account-recovery code runs only on `Err(Failure)`, including selected HTTP 400 messages. However, none of `Fail::Never`, `Fail::Limit`, or `Fail::Fallback` takes HTTP 400. The reported `no Gemini models available; refresh the authenticated session` response is forwarded instead of returned to the recovery branch.

For a disconnected last/only Gemini route, `Fail::Never` and `Fail::Limit` similarly write the connection failure as HTTP 502 and return `Ok(())`. The automatic `ensure_bridge` call is never reached in that ordinary cold-start scenario.

Required correction: separate provider request/recovery from committing a response to the client. Startup and recoverable session errors must be handled before the generic final-response policy.

### 3. P1 — In-stream errors also bypass account/provider failover

Evidence: `src/proxy/mod.rs:541`; `src/proxy/http.rs:313-341`; original Argon `bridge-source/internal/modules/claude/claude_controller.go:108-130`.

The Go bridge opens streaming responses with HTTP 200 and can subsequently emit an SSE `error` when generation fails. ccat immediately commits the HTTP 200 and copies the bytes without interpreting that event. A Google session or quota failure before any model content is therefore treated as a successful forwarding operation, with no account or provider fallback.

Required correction: parse provider events and retain a pre-content recovery boundary. Never restart a response after actual model content has been delivered.

### 4. P1 — Any listener can be mistaken for the selected Google account

Evidence: `src/providers/gemini_web.rs:335-336`, `src/providers/gemini_web.rs:366-374`.

Bridge startup and readiness accept a listening TCP port as success. If an original Argon instance, another account's bridge, or an unrelated process already occupies that port, ccat does not establish process ownership or Google identity before using it. The selected account's credentials are not applied to that existing process.

Required correction: remove sidecar-port account identity from native routing. Until then, a listening socket alone must not count as a managed authenticated account.

### 5. P1 — Re-login does not update a running bridge's credentials

Evidence: `src/providers/gemini_web.rs:669-670`, `src/providers/gemini_web.rs:339-353`, `src/providers/gemini_web/login.py:155-161`.

Re-login launches Python and rewrites the DPAPI file. An existing bridge retains the cookie supplied through `GEMINI_COOKIES` when it was spawned. ccat neither monitors login completion nor reloads/restarts that authenticated client. Because the port is still online, `ensure_bridge` returns without applying the fresh credential.

Required correction: a completed native login must replace the relevant account session and invalidate its stale in-memory authentication, with completion/errors surfaced in the UI.

### 6. P1 — A clean ccat installation cannot provision the Go bridge

Evidence: `src/providers/gemini_web.rs:48-85`.

`ensure_installed` copies the binary only from the developer-specific `D:\Dev\ccat\bin\gemini-api.exe` location. The other lookup paths require a previously installed AppData copy or a manually adjacent executable. The Rust package does not build or embed the Gemini implementation. Installing only `ccat.exe` on a clean machine leaves Gemini unable to start.

Required correction: implement the Gemini backend in ccat rather than relying on an undeclared developer-machine companion binary.

### 7. P1 — Sign-in requires undeclared external runtimes and hides failures

Evidence: `src/providers/gemini_web.rs:380-399`, `src/providers/gemini_web/login.py:127`, `src/providers/gemini_web/login.py:139-153`, and sign-in/re-login button handlers in the Gemini settings page.

ccat invokes whichever `python` is on PATH, imports external `playwright.sync_api`, and launches the installed Chrome channel. It provisions none of these prerequisites. A clean ccat installation can fail before displaying Google login. UI handlers discard launch errors and do not monitor the child process's eventual failure.

Required correction: ccat-owned browser sign-in/session handling, explicit supported-browser detection, and actionable asynchronous login results. Copying a Python script is not a native login implementation.

### 8. P1 — Static model labels do not implement manual model routing

Evidence: `src/settings/providers.rs:60-67`; original Argon `bridge-source/internal/modules/providers/gemini_models.go:313-345` and `gemini_service.go:532-568`.

Rust now advertises four fixed model names, but the unchanged Go bridge still validates each name against authenticated `cachedModels`. Generation requires the internal model ID, capacity fields, and model number that the bridge obtains through authenticated discovery. A fixed picker list supplies none of that metadata. If discovery failed, the same manually selected name still yields the original HTTP 400.

Required correction: implement actual Gemini Web model selection/routing in ccat, consistent with the user's manual-selection requirement. A display name must not be confused with the protocol's routing identity. Authentication must remain a separate prerequisite, not be faked by an always-present model list.

### 9. P1 — Deleting an account does not stop using that account

Evidence: `src/providers/gemini_web.rs:262-270`, `src/main.rs:1836`, and the Gemini forwarder's URL-based routing.

Deleting an account removes its registry entry and DPAPI file but leaves the detached authenticated bridge running and leaves the provider URL unchanged. `target` does not require an enabled Gemini account. Existing sessions can therefore continue sending prompts to the removed Google identity.

Required correction: live account-pool changes must make a removed/disabled account immediately ineligible and release its owned session resources. Deleting a credential file alone does not revoke a running client.

### 10. P2 — Connection state is represented as fabricated quota percentages

Evidence: `src/providers/gemini_web.rs:427-486`.

A listening account with a nonempty cached model list is assigned `used = 0`; an offline or allegedly expired account is assigned `used = 100`. No Google allowance, consumption, or reset was measured. These values are rendered through the shared usage-bar interface as if they were usage limits. The model list is cached discovery data, not a current authentication or quota check.

The bars have `window = 0`; they also do not provide real five-hour or weekly quota measurements for the tray. Health, authentication, model availability, and quota are distinct states. Offline/empty-model responses must not automatically become either “quota exhausted” or “session expired.”

Required correction: show only limits/reset values supported by a verified source. When Google supplies no measurable quota data, show “quota unavailable/unknown,” separately from account readiness and observed rate-limit responses.

## Native migration and feature-parity gaps

Original Argon documentation (`CHANGELOG.md:5-8` and the usage guide's dashboard section) describes enabled/preferred accounts, round-robin and least-used scheduling, per-account request/error/latency history, playground and single/bulk checks, concurrency/RPM limits and FIFO queueing, and cookie-free configuration import/export.

The current ccat Gemini page offers name/sign-in, URL selection, Start/Stop/Delete, and coarse status. It does not provide native parity for those scheduling, queueing, import/export, testing, and per-account history features. The reduced account schema also omits original counters. Existing ccat provider-level analytics is not a replacement for the original per-account history: the generic forwarder records `acct = ""` at `src/proxy/http.rs:339`.

The copied executable retains additional APIs for images, audio/music, video, Canvas, uploads, and Deep Research. Their presence in that executable is not evidence that corresponding first-class ccat workflows were migrated. There is also no versioned asset-update mechanism: copying an asset only when absent preserves stale helpers indefinitely.

## Fresh runtime evidence

The independent Opus runtime workflow performed read-only local probes and one existing test-suite run. It did not decrypt credentials or send a generation request.

| Check | Observed result |
| --- | --- |
| `cargo test --bin ccat` | Exit 0: **240 passed, 6 ignored**, 2.04 s |
| `GET http://127.0.0.1:4982/health` | HTTP 200, status `ok` |
| `GET http://127.0.0.1:4982/claude/v1/models` | HTTP 200, four listed model IDs |
| `GET http://127.0.0.1:4983/health` | HTTP 200, status `ok` |
| `GET http://127.0.0.1:4983/claude/v1/models` | HTTP 200, empty `data` |
| Installed `ccat.exe` versus existing `target/release/ccat.exe` | Byte-identical, 18,719,232 bytes |
| Repo, AppData, and original Downloads `gemini-api.exe` copies | Byte-identical, 15,145,984 bytes |
| Process origins | Both an original Downloads Argon executable and an AppData Gemini executable remain running |

Bridge SHA-256, identical for all three copies:

```text
1EC05B81203483A43FEA3EA02C5886EF621DC39C8FAF2DB4CB9A506CB4775E5C
```

Installed/existing-release ccat SHA-256:

```text
08BAAE58FEC3149BB28C540F0FE3E69F6F7988FDB7279311F32944F13DF031C2
```

Matching installed/release hashes do not prove that the binary contains every current uncommitted source change. Endpoint-to-process attribution is not needed for this verdict and is not asserted for every port.

## What was not verified

The passing suite includes synthetic Gemini DPAPI, URL, port, static-model-list checks and a canned TCP forwarding test. It does not establish a real Google login-to-generation flow. The six ignored integration checks are not evidence of live Gemini Web coverage.

This review did **not** exercise new Google login, cookie refresh, account removal during generation, real chat/tool/multimodal requests, streaming errors/cancellation, recovery/failover, clean-machine installation, UI interaction, or frame-rate measurement. No source-code fixes, installs, account modifications, bridge launches/restarts, or process termination were performed.

## Correct direction

A genuine migration needs a ccat-owned Gemini Web backend: account/session lifecycle and secure credentials, real protocol model routing, generation and stream translation, provider/account retry boundaries, truthful usage reporting, and verified feature parity. Its account UI must not require Bridge URL, API keys, or manual sidecar management. Retain the existing user's account data safely; do not silently substitute another identity or claim a successful migration based on mocked tests.

Acceptance must include a clean installation with the old Argon folder absent, authenticated generation and tool round trips through ccat, cold start, re-login, account switch/remove/disable, pre-content versus mid-stream failures, usage/unknown-quota semantics, and UI responsiveness. Those are acceptance requirements, not claims that they currently pass.
