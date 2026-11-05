# OpenCode Egress Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Rebuild the OpenCode special provider with policy-respecting pooled egress and an honest pool-focused settings page.

**Architecture:** Keep ccat's current local Anthropic server and reusable SSE/conversion code. Replace only OpenCode's fake CLI layer, add a typed per-provider egress pool and expose its configuration/runtime snapshots through the existing settings window.

**Tech Stack:** Rust, existing egui/eframe/serde, reqwest blocking + Rust TLS + SOCKS.

**Spec:** `docs/superpowers/specs/2026-11-04-opencode-egress-design.md`

## Global Constraints
- Current branch; no standalone server, no auto-commits, no Bun dependency, no unrelated rewrites.
- No IP rotation on admission, quota, HTTP response/read failure, ambiguous delivery or committed output.
- Caller tools and local-auth isolation are release blockers. No fabricated CLI identity, reasoning signature or exact token count.
- Settings shows pool addresses, not provider/server IPs; no external IP-echo requests.
- Full existing test suite and Windows release build/install; report any unavailable platform/live checks.

## Review Focus
- A changing pool configuration while streams are active must not terminate those streams or leak clients/threads.
- Concurrent sessions and subagents must not share identities or override sticky routes.
- A timeout after request bytes were delivered must not replay inference on another IP.
- A 403/429 must remain an upstream error, not make a transport node unhealthy.
- Secrets in URL userinfo, missing secret references and arbitrary upstream bodies must not appear in state/logs.

### Task 1: Native pooled egress
**Files:** Create `src/providers/opencode/egress.rs`; modify `Cargo.toml`/generated lockfile.
**Interfaces:** `Config`, `NodeConfig`, `Kind`, `Snapshot`; `send(scope, config, affinity, health_url, url, body, headers) -> Result<Reply, Failure>`; `snapshots(scope) -> Vec<Snapshot>`; `validate(config) -> Result<(), String>`.
- [ ] Write unit/mock tests for deterministic weighted affinity, independent scope, draining, invalid address/credentials, connection-only failover, no rotation on 403/429 and streamed resource release.
- [ ] Run tests to establish failures before implementation.
- [ ] Implement reusable clients, bounded concurrency, short state locks, background public-catalog health checks and credential-free per-process snapshots.
- [ ] Test direct, bind-IP, local HTTP/SOCKS proxy paths; verify stream drop releases the active count.

### Task 2: Provider transport and protocol fidelity
**Files:** `src/providers/opencode.rs`, `src/providers/openai.rs`, `src/providers/mod.rs`, `src/proxy/mod.rs`, `src/proxy/http.rs`, `src/proxy/sse.rs`, `src/main.rs` as necessary.
**Interfaces:** `Provider.opencode_egress: egress::Config`; dedicated `Upstream::OpenCode` preserves provider URL/pool configuration and is not a paid endpoint; OpenCode `messages` accepts request headers/context for scope/session affinity.
- [ ] Add regressions proving caller tools are not renamed/injected, query routes work, token counting does not invoke inference, and free-provider rejection is not silently held.
- [ ] Remove old harness/manifest/identity emulation and response-side tool renaming; retain catalog/filter and config migration.
- [ ] Connect the OpenCode upstream to Task 1, keeping ordinary OpenAI endpoints unaffected.
- [ ] Route explicitly declared Chat/Responses/Anthropic protocols using existing converters/pass-through; validate unsupported capabilities and free-only model policy.
- [ ] Preserve safe retryability/error information, complete SSE/tool IDs and accurate usage; fix only verified shared defects.
- [ ] Run protocol and existing provider tests.

### Task 3: Pool-focused settings and config
**Files:** Create `src/providers/opencode/ui.rs`; modify `src/providers/endpoint.rs`, `src/settings/mod.rs`, `src/settings/providers.rs`, configuration migration/tests as required.
**Interfaces:** `opencode::page(editor, ui, config, feed, index)` reuses common controls; `egress::snapshots(provider_name)` supplies real runtime display.
- [ ] Add tests for default Direct pool, persisted pool edits, invalid config and distinct built-in settings page.
- [ ] Implement URL + add/edit/remove/enable/drain pool controls and configured IP/proxy/weight/secret references; no API-key requirement or provider IP display.
- [ ] Render measured status/counts/latencies with honest idle/stale states.
- [ ] Verify live config reload keeps existing streams and other providers intact.

### Task 4: Verify, document and install
**Files:** `README.md`, `IMPLEMENTATION_DECISION.md`, this plan and design.
- [ ] Record audit findings/implementation map, configuration example, supported protocol/capability policy and documented upstream limits.
- [ ] Run `cargo test`, `git diff --check`, protocol mock tests; bounded live catalog check only unless inference is explicitly available.
- [ ] Build `cargo build --release`; install via existing safe binary backup/swap and verify file hash/version.
- [ ] Open installed Settings, interact with pool controls without changing unrelated saved routes, capture/review a screenshot.
- [ ] Attempt Linux checks using an available Linux toolchain; report skipped/unavailable checks truthfully.

Approval is automatic under the user's project instructions. Implementation follows this plan without an additional approval gate; no commits or branches are authorized.
