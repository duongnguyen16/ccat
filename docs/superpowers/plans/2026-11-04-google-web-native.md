# Native Google Web Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the Argon executable/Python wrapper with a first-class ccat-native Google Web provider, preserving account data and source capabilities.

**Architecture:** Native Rust worker-thread Google HTTP client, private per-account sessions and persistent scheduler/history. Existing ccat proxy/SSE/gateway and account UI patterns remain the integration layer; no provider-configured network bridge.

**Tech Stack:** Existing Rust stdlib, ureq 3.4.2, serde/serde_json, tungstenite 0.26.2, base64, Windows DPAPI, egui.

**Spec:** `docs/superpowers/specs/2026-11-04-google-web-native-design.md`

## Global Constraints

- Runtime independence from Argon directory, gemini-api.exe, Python and Playwright.
- Preserve old account/credential/profile data; no destructive process or account cleanup during migration.
- No URL/key requirement or manual server lifecycle; no cross-provider credential forwarding.
- User-selected models remain explicit; unsupported/unknown routing cannot silently choose another model.
- Unknown quota remains unknown; no status-derived usage percentages.
- All potentially slow operations off the egui thread; private files and redacted errors.
- Current branch only; no commits/pushes/merges. Design/plan are auto-accepted by the user's global instructions.

## Review Focus

- Existing AppData profiles and old registry fields survive migration and re-login.
- Cold start/failed authentication/empty metadata do not become false readiness or generic invalid-model errors.
- Failover occurs only before a response starts; disabled/deleted identities never receive new requests.
- Unexpected Google frames, unknown tool names and external media URLs fail safely without leaking cookies.
- UI action errors and missing-browser/login timeouts remain visible without blocking a frame.

### Task 1: Native Gemini protocol

**Files:** Create `src/providers/gemini_web/wire.rs`.
**Consumes:** Existing `crate::common::Failure`, `crate::common` IDs; supplied Go provider source and tests.
**Produces:** `Model`, `InputFile`, `Session`, `Reply`; `initialize(cookie: &str, auth_user: &str) -> Result<Session, Failure>`; `generate(session: &mut Session, model: &str, prompt: &str, files: &[InputFile], options: &serde_json::Value) -> Result<Reply, Failure>`. Session exposes authenticated metadata and trusted HTTP/form helpers for media operations.

- [ ] Port model/status frames, routing headers, generation payload and response/errors with bounded reads and no secret-bearing error URLs.
- [ ] Port/upload bounded inline files and preserve generated media/Canvas information in Reply.
- [ ] Add fixture checks for real source shapes, account capacity, malformed/error/empty responses, explicit model identity and upload URL trust boundaries.
- [ ] Run integrated Rust checks when root modules are wired; never infer Google compatibility from fixtures.

### Task 2: Native accounts and browser login

**Files:** Create `src/providers/gemini_web/store.rs`, `browser.rs`.
**Produces:** `Account` (serde-compatible registry metadata), `list()`, `save(&[Account])`, `cookie(id)`, `save_cookie(id, value)`, `settings()/save_settings`, private runtime/history state and cookie-free import/export. Browser `login(account: &Account, headless: bool, cancel: &AtomicBool) -> Result<(), String>` owns its isolated supported browser and saves native DPAPI credentials.

- [ ] Add data-preservation/ID/path/DPAPI/private-file checks before store implementation.
- [ ] Implement serialized atomic mutations, unknown-field retention, profile-path compatibility, native DevTools login/refresh, cancellation and errors; no Python.
- [ ] Preserve enabled/preferred/strategy/concurrency/RPM data and request history. User data is never logged as prompts/cookies.
- [ ] Add synthetic DevTools message/cookie filtering tests without opening a browser in unit tests.

### Task 3: Conversation/tool conversion

**Files:** Create `src/providers/gemini_web/messages.rs`.
**Consumes:** `wire::{InputFile, Reply}` and existing proxy tool helpers.
**Produces:** `build(body: &Value) -> Result<(String, Vec<InputFile>), Failure>` and `finish(body: &Value, reply: &Reply, shown: &str) -> Result<Value, Failure>`.

- [ ] Tests cover tools/tool results, text/images/documents, thinking, tool choice, fenced JSON, argument validation and disallowed tool names.
- [ ] Port source conversation and tool-bridge behavior; reuse normal ccat Anthropic response structure and IDs. No fabricated tool arguments.
- [ ] Count/label estimated usage honestly, validate response stop conditions and preserve actual versus requested model metadata.

### Task 4: Source media/research parity

**Files:** Create `src/providers/gemini_web/media.rs`.
**Consumes:** Native `wire::Session` and its authenticated HTTP helpers; source `gemini_canvas.go`, `gemini_music.go`, `gemini_video.go`, `deep_research.go`.
**Produces:** Native operations dispatched from ccat's provider/MCP layer, with documented JSON operation/result shapes, no local bridge calls.

- [ ] Port mode payload changes, generated-media extraction/download, video job/research operations and source fixtures.
- [ ] Reuse wire types; protect download hosts, paths and sizes. Do not generate remotely in tests.
- [ ] Expose operation errors/unsupported upstream behavior explicitly rather than stubbing success.

### Task 5: Native pool, first-class UI and routing

**Files:** Replace `src/providers/gemini_web.rs`; modify `src/proxy/mod.rs`, `src/main.rs`, `src/settings/providers.rs`, `src/usage.rs`, appropriate MCP/gateway integration; remove obsolete wrapper source/assets from use.

- [ ] Wire tasks 1–4, migrate stored URL/key to account pool selection, keep stored provider identity backward compatible.
- [ ] Implement FIFO slots, account preference/strategy/RPM/cooldown/history and per-account native session refresh. No retry after client output.
- [ ] Replace bridge UI with account-driven enable/preference/re-login/check/remove controls and asynchronous jobs; include source settings/import/export/checks/playground capabilities.
- [ ] Use ccat SSE output helpers with valid thinking/text/tool blocks and account analytics; separate quota unknown from session state.
- [ ] Add regression checks for all ten prior review findings; fixture/mock checks must leave user accounts untouched.

### Task 6: Verify, review, build, install

**Files:** README/provider documentation, MIT attribution, `IMPLEMENTATION_DECISION.md` appended entries and tests.

- [ ] Run all Rust tests, focused native protocol/account/proxy tests, and source scans for old runtime dependencies.
- [ ] Independent Opus review of actual implementation; resolve confirmed defects and rerun relevant checks.
- [ ] Test a minimal real native login/session/generation path if existing authentication permits; report user-interaction blockers accurately.
- [ ] Build release once code is verified, install with a unique backup filename without terminating live sessions; verify installed hash/version.
- [ ] Report remaining upstream/interactive limitations, not blanket feature-complete or FPS claims.

Tôi sẽ chuyển từng phần từ mã nguồn Argon vào ccat rồi kiểm tra chúng cùng nhau. Dữ liệu tài khoản cũ được giữ lại, còn lớp cầu nối Go/Python sẽ không còn được sử dụng. Sau khi kiểm tra và rà soát, bản mới sẽ được build và cài trực tiếp trên máy.
