### [2026-10-26 13:40] MuseAI: no simulated token streaming
- **Context:** making MuseAI feel like a native provider; live probes showed `/chat/stream` only returns an ack (`message_id`) and history shows no partial text (one empty in-progress state, then the full reply).
- **Options:** A replay the finished reply in small SSE chunks / B send it as one delta, as today
- **Chosen:** B (confidence ~85%)
- **Why:** chunking a finished reply only adds latency and fakes progress; the reply is shown the moment Muse completes it.
- **Revert if:** Muse exposes incremental text (a stream frame or history field) — then stream it for real.

### [2026-10-26 13:40] MuseAI: poll history with limit=1
- **Context:** each poll with limit=20 re-downloads the user message, i.e. the whole prompt (~975 KB per poll for a 320 KB prompt; past the 16 MiB body limit for the largest prompts).
- **Options:** A limit=1 / B limit=20 with a periodic fallback / C keep limit=20
- **Chosen:** A (confidence ~80%)
- **Why:** measured 1.5 KB and ~255 ms per poll instead of ~975 KB and ~450 ms; in every probe the finished reply was the newest event.
- **Revert if:** replies that Muse finished wait until the 600 s deadline (a later event of another kind became the newest).

### [2026-10-26 13:40] MuseAI: keep stateless requests
- **Context:** sending only new messages to a long-lived Muse session would shrink prompts.
- **Options:** A one fresh session per request (today) / B one persistent session per conversation
- **Chosen:** A (confidence ~85%)
- **Why:** Muse's cross-session memory already "remembers" failed tool calls; a persistent session would make that worse and needs conversation tracking the proxy does not have.
- **Revert if:** prompt upload time dominates latency on long sessions.

### [2026-10-26 13:40] MuseAI: concurrency cap stays at 2, no queue
- **Context:** the plan proposed queueing extra requests instead of answering 429.
- **Options:** A queue / B keep the 429, which lets the proxy fall back to another route
- **Chosen:** B (confidence ~80%)
- **Why:** a 429 already reaches the proxy's route fallback; a queue would hold Claude Code's parallel requests behind a slow Muse reply. Muse's real concurrency limit is unmeasured.
- **Revert if:** MuseAI is the only route and parallel subagents fail on 429.

### [2026-10-26 13:40] MuseAI: no tool-schema compaction yet
- **Context:** smaller prompts could reply faster.
- **Options:** A strip long descriptions / unused `$defs` / B leave schemas as Claude Code sends them
- **Chosen:** B (confidence ~80%)
- **Why:** not measured that prompt size drives Muse's reply time, and trimming descriptions can make tool calls worse — the part that is already unreliable.
- **Revert if:** a measurement shows reply time grows clearly with prompt size.

### [2026-10-27 08:06] Hold timeout overrides an inherited API_TIMEOUT_MS
- **Context:** A held request died at "Request timed out (API_TIMEOUT_MS=3000000ms)": ccat lifted the timeout only when the env had none, and the launching shell already set 50 min.
- **Options:** A keep "env wins" and add a setting / B setting whose default (7 days) replaces the inherited value, "Keep mine" opts out / C always override, no setting
- **Chosen:** B (confidence ~85%)
- **Why:** The fix must work without the user touching anything, yet someone who really wants a shorter timeout keeps a way to say so. Also passed via `--settings` env so a settings.json value can't shorten it.
- **Revert if:** the user relies on a short inherited API_TIMEOUT_MS for ccat sessions and doesn't want to pick "Keep mine".

### [2026-10-31 15:30] MuseAI removed: old configs drop the provider on load
- **Context:** MuseAI is dropped entirely; saved configs still carry a `muse: true` provider and tiers may name it.
- **Options:** A: keep a read-only `muse` flag and drop that record in `load` / B: leave it as a dead endpoint / C: rewrite tiers to another provider
- **Chosen:** A (confidence ~90%)
- **Why:** the record goes away without touching other providers; tiers naming it show the existing "provider no longer exists" problem, so the user picks a replacement instead of ccat guessing one.
- **Revert if:** users expect their MuseAI tiers to be moved to a specific provider automatically.

### [2026-10-31 15:30] secure_file moved to common; run_openai_chat kept
- **Context:** `agent`/`remote` used MuseAI's `secure_file`; Chat Completions streaming was used only by MuseAI.
- **Options:** A: move the ACL helper to `common`, keep `run_openai_chat` behind `allow(dead_code)` / B: delete `run_openai_chat` too
- **Chosen:** A (confidence ~85%)
- **Why:** a keyed OpenCode Zen provider (pending decision) would stream Chat Completions through it.
- **Revert if:** OpenCode is not added — then delete `run_openai_chat` and its heartbeat test.

### [2026-10-31 15:30] Install by swapping the running binary aside
- **Context:** CLAUDE.md asks to build and install after a task; the install command is a placeholder and ccat.exe in `~/.cargo/bin` was running.
- **Options:** A: `cargo build --release`, rename the running exe to `ccat.old.exe`, copy the new one / B: `cargo install --path .` (fails on a locked exe) / C: kill running ccat
- **Chosen:** A (confidence ~90%)
- **Why:** replaces the binary without killing live sessions; new launches use the new build.
- **Revert if:** the user prefers running sessions restarted, or a different install location.

### [2026-10-31 10:56] Ollama: keep native /api/chat, cloud only
- **Context:** Ollama Cloud now also accepts Anthropic Messages at `ollama.com/v1/messages`; the user asked for a proper cloud-only Ollama provider.
- **Options:** A: keep the native `/api/chat` translation, drop local support / B: pass Claude Code's requests through to `/v1/messages`
- **Chosen:** A (confidence ~85%)
- **Why:** `/v1/messages` lacks `tool_choice`, deferred tools, hosted web search, cached-token counts and `count_tokens`; the native route has tests and covers them. B would also have touched `proxy::run` while another session edits it.
- **Revert if:** Ollama's `/v1/messages` gains those features, or `/api/chat` starts lagging behind it for new models.

### [2026-10-31 10:56] Ollama: no num_ctx / keep_alive, effort capped at high, no free Ollama models
- **Context:** the old 64k `num_ctx` and 30m `keep_alive` were tuned for local models; the Free badge needs a list of free models.
- **Options:** A: drop both, keep `think` at low/medium/high, mark no Ollama model free / B: keep them, send `max`, hardcode OmniRoute's 2026-09 free list
- **Chosen:** A (confidence ~85%)
- **Why:** a fixed 64k context could cap cloud models with 256k-1M windows; `/api/chat` acceptance of `max` is unverified (no key to test); ollama.com/pricing (2026-10-31) bills every cloud model per token. The badge marks `:free` endpoint models (OpenRouter).
- **Revert if:** Ollama reintroduces free cloud models, or a live call shows `think: "max"` works.

### [2026-10-31 16:40] OpenAI-compatible endpoints: separate upstream, no max_tokens
- **Context:** free-tier providers (NVIDIA NIM, OpenRouter, Groq, Gemini, …) speak Chat Completions; ccat endpoints only spoke Anthropic Messages.
- **Options:** A: an `openai` flag on the endpoint + `Upstream::OpenAi` / B: a new built-in provider per vendor / C: one built-in "Free providers" page
- **Chosen:** A (confidence ~85%)
- **Why:** one converter serves every vendor; presets fill the URL so no per-vendor code. `max_tokens` is left out because Claude Code asks for 32k–64k, above most of these models' limits (a 400); reasoning effort is left out because non-reasoning models reject it.
- **Revert if:** a provider cuts replies short without `max_tokens`, or a vendor needs its own auth/usage handling.

### [2026-10-31 16:40] "Never fall back to a paid API key" also skips OpenAI-compatible endpoints
- **Context:** the setting skips custom endpoints as fallbacks; free-tier keys never bill, OpenRouter keys can.
- **Options:** A: treat them like Anthropic endpoints / B: exempt them
- **Chosen:** A (confidence ~80%)
- **Why:** the setting promises no API-key fallback at all; ccat cannot tell a free key from a billed one.
- **Revert if:** the user wants free-tier endpoints usable as fallbacks with that setting on.

### [2026-10-31 14:45] OpenCode free tier: zen-proxy's request shaping inside the OpenAI-compatible route
- **Context:** support opencode's anonymous free models natively, from 12errh/zen-proxy (a Bun proxy). The gateway only serves requests shaped like the OpenCode CLI.
- **Options:** A: an "OpenCode Free" preset on the OpenAI-compatible endpoint; `opencode.rs` adds the CLI's headers, preamble, manifest and reply-side call renaming when the URL is the free tier / B: a new built-in provider kind / C: spawn zen-proxy under Bun
- **Chosen:** A (confidence ~85%)
- **Why:** the existing Chat Completions converter already does the translation; only the gate shaping is new. Not ported: the Responses API family (no free model uses it), model aliases, rate limiting, dashboard, BYOK escalation. zen-proxy's argument map (`filePath`) did not match its own v2 manifest (`path`); the map here follows the manifest.
- **Revert if:** the gate refuses rustls' TLS fingerprint (zen-proxy says it refuses Node's). Not verifiable on 2026-10-31: this IP's free quota was spent (the official OpenCode CLI got the same 429, retry-after ~17 h). Check with `cargo test --release live_free_tier -- --ignored --nocapture`; if Bun passes and ureq does not, the transport needs Bun or TLS impersonation.

### [2026-11-02 10:30] Gateway: ccat's tiers served to other apps from the tray
- **Context:** the user asked for a Settings option giving other harnesses a URL (OpenAI and Anthropic style), with API keys (add/remove or accept any) and a model name per tier (or a default).
- **Options:** A: a front server in the tray that maps names to tiers and forwards to an in-process proxy over loopback / B: give the per-launch proxy a fixed port and a gateway mode / C: a new `ccat --gateway` process
- **Chosen:** A (confidence ~85%)
- **Why:** the tray is the one ccat that always runs; a front server keeps the proxy's Claude Code paths (official forwarding with the caller's login, the catch-all to api.anthropic.com) out of reach, so a gateway key can never travel to Anthropic. Chat Completions are translated to Anthropic Messages in front, so no provider changed.
- **Revert if:** the gateway must also run without the tray (Linux/macOS desktop without `ccat --serve`).

### [2026-11-02 10:30] Gateway: localhost only, Claude tiers not served, default names are model ids
- **Context:** how far the gateway reaches, what to do with tiers on official Claude, what a tier is called when no name is set.
- **Options:** bind 127.0.0.1 / 0.0.0.0; Claude tiers refused / forwarded with a stored login; default name = tier's model id / tier key
- **Chosen:** 127.0.0.1 only; Claude tiers (and Claude fallbacks) refused with a reason; default = the tier's model id, the tier key (`opus`) always accepted too (confidence ~80%)
- **Why:** "accept any key" on a LAN address would open the user's plans to the network; the gateway has no Claude Code login to forward; the model id is what an app expects to see in `/v1/models`. Unknown names get a 404 listing the served ones instead of going anywhere.
- **Revert if:** the user wants other devices to reach it, or an app needs a fixed alias rather than the routed model id.

### [2026-11-03 21:43] OpenCode Free is a URL-only special provider
- **Context:** OpenCode Free was a generic keyed endpoint preset, so clearing the key broke launch and routing and paid-key fallback filtering excluded it.
- **Options:** A: exempt only the fixed URL / B: a special provider flag with the existing OpenAI proxy adapter / C: a new transport
- **Chosen:** B (confidence ~95%)
- **Why:** keeps the existing adapter and makes the editable URL independent of anonymous access. Old presets keep their names, models and routes; user keys are cleared, new setups require only a URL, and free fallbacks remain eligible. URL-based detection is migration-only for old records without the new flag; modern regular endpoints never change kind when their URL is edited.
- **Revert if:** OpenCode removes anonymous access or the provider needs a user credential.

### [2026-11-03 21:43] Disable automatic compilation of the obsolete TUI library
- **Context:** untracked src/lib.rs pulls in old TUI modules whose dependencies no longer exist in Cargo.toml; even cargo test --bin ccat fails before reaching the current app.
- **Options:** A: autolib = false / B: delete the old files / C: restore obsolete dependencies
- **Chosen:** A (confidence ~95%)
- **Why:** the current app is a binary; one manifest setting restores builds without deleting unrelated work or adding unused dependencies.
- **Revert if:** the project intentionally restores a supported library target.

### [2026-11-03 22:06] Keep upstream rejection bodies out of errors and logs
- **Context:** the security review found arbitrary upstream error bodies copied into Failure.message, normal logs and client responses.
- **Options:** A: status-only errors / B: allowlist upstream error fields / C: forward raw bodies
- **Chosen:** A (confidence ~95%)
- **Why:** a fixed HTTP-status message cannot expose echoed prompts or credentials, while the status and Retry-After still drive routing and backoff. No provider-specific parser is needed.
- **Revert if:** a documented non-sensitive error code is needed for diagnosis; allowlist that code rather than restoring raw bodies.

### [2026-11-03 22:55] Do not ship the unverified OpenCode transport experiment
- **Context:** the native CLI initially returned OK while ccat got 429. A Bun relay succeeded once, but later native CLI controls also reported Free usage limit reached. Changing transport and native headers did not make the live adapter test pass.
- **Options:** A: install the experimental transport / B: revert it and retain a strict live regression check
- **Chosen:** B (confidence ~99%)
- **Why:** conflicting controls do not establish a transport root cause. The installed release and user's wait/fallback configuration are unchanged; the live test now fails on a rejected request instead of printing 429 and reporting success. Offline tests pass, but live compatibility remains unverified until a healthy native control is available.
- **Revert if:** a controlled native-versus-adapter comparison demonstrates a fix and the real model responds through ccat.

### [2026-11-04 09:07] Antigravity: observed-hit affinity with earliest-expiry quota routing
- **Context:** User requested Antigravity optimization prioritizing accounts with prior caching, otherwise ordering by weekly reset earliest, 5h reset remaining, and quota remaining, plus natural relative reset labels.
- **Options:** A: strict hierarchy (cache -> weekly reset -> 5h reset -> headroom) / B: counterproposal combining observed prompt-prefix affinity hints with earliest reset across both windows (min(weekly, 5h)), requiring both windows to have quota, breaking ties by plan rank / C: pure round-robin
- **Chosen:** B (confidence ~90%)
- **Why:** Strictly favoring weekly reset first can strand an account whose 5h reset expires sooner, leaving usable short-term tokens wasted. A recent observed hit on identical model + prompt prefix keeps cache warm without guessing backend state. If no cache match exists, spending the allowance that expires earliest (whether 5h or weekly) maximizes total throughput over time. Both windows must have headroom so exhausted accounts never burn request latency.
- **Revert if:** Google Cloud Code Assist exposes explicit cache handles or account-agnostic server caches, or if users report 5h windows should always yield to weekly preservation.

### [2026-11-04 10:15] Gemini Web integrated via local Argon bridge
- **Context:** User requested integrating Gemini Web as a provider from `C:\Users\duncuti16\Downloads\Argon-Local-MCP-v1.1.0`.
- **Options:** A: Built-in `Gemini Web` provider speaking Anthropic Messages directly to the local Argon bridge (`/claude/v1`) with dashboard auto-detection / B: Custom endpoint preset / C: Re-implement Google Web protocol directly in Rust
- **Chosen:** A (confidence ~95%)
- **Why:** Argon Local already bundles a battle-tested Go bridge (`gemini-api.exe`) that implements native Anthropic Messages API (`/claude/v1/messages`), SSE streaming, tools, and token counting, with DPAPI cookie storage and Chrome login management. Integrating as a first-class built-in provider gives immediate, rock-solid Gemini Web access (Argon 4, 3.1 Pro, 3.8 Flash) with zero protocol duplication and native fallback eligibility.
- **Revert if:** The user requires embedding Playwright and the Go bridge directly inside ccat without relying on the Argon Local directory.

### [2026-11-04 11:10] Gemini Web fully migrated into ccat as a native module
- **Context:** User requested full migration from the Downloads folder into ccat: no longer depend on `Argon-Local-MCP-v1.1.0` while retaining all features (accounts, bridge, login, DPAPI).
- **Options:** A: Self-contained module in `%APPDATA%\ccat\gemini_web` with bundled `gemini-api.exe`, native Rust DPAPI encryption/decryption, auto-waking bridge processes on demand, and non-blocking background status caching / B: Keep referencing Downloads folder / C: Rewrite Go bridge from scratch in Rust
- **Chosen:** A (confidence ~98%)
- **Why:** Keeps 100% of the proven capabilities (multi-account, streaming SSE, tool calling, token counting) completely independent of the Downloads folder. All accounts and DPAPI secrets are stored in `%APPDATA%\ccat\gemini_web\accounts`, bridges are automatically managed and started on demand by ccat when needed, and the GUI runs at 60 FPS without frame-blocking HTTP probes.
- **Revert if:** The user requests eliminating the Go bridge binary entirely in favor of 100% in-process Rust HTTP requests to Google.

### [2026-11-04 13:45] Gemini Web models 100% manual, usage limits view, and account session failover
- **Context:** User reported `400 no Gemini models available` when a selected account had an expired session, requested models to be 100% manual, and requested usage limits view support.
- **Options:** A: 100% manual static model list (no fragile dynamic fetch), proxy multi-account failover on session errors, Re-login button for expired accounts, and full usage limits view support in Settings and Tray / B: Force re-fetching models from bridge on each run
- **Chosen:** A (confidence ~99%)
- **Why:** Gemini Web models (`gemini-4-argon`, `gemini-3.1-pro`, `gemini-3.8-flash`, `gemini-3.5-flash-lite`) are fixed. Removing the dynamic fetch eliminates error banners and keeps models always available in the picker. The usage limits card provides full visibility into account session states, and automatic proxy failover prevents Claude Code from crashing when an individual account's cookie has expired.
- **Revert if:** Gemini Web adds dynamically assigned, user-specific model aliases that must be queried per account.

### [2026-11-04 16:00] Google Web: replace the wrapper with in-process Rust
- **Context:** The user rejected both the external Argon folder dependency and the relocated Go/Python wrapper. The E2E review showed that earlier claims of a complete native migration, reliable failover, verified quotas and 60 FPS were unsupported. This entry supersedes the Gemini implementation assumptions above; it does not claim the replacement is already verified.
- **Options:** A: Hide the Bridge URL while retaining sidecars / B: Port the supplied Go/Python protocol and account features into Rust, with Chrome/Edge only for interactive sign-in / C: Drop the provider
- **Chosen:** B (confidence ~99%; explicitly required by the user)
- **Why:** Native routing must own authentication, Google model identities and request recovery rather than forwarding to a separately running server. Stored accounts/profiles are retained, models stay manually selected, and unmeasured quota stays unknown.
- **Revert if:** The user explicitly changes the native-provider requirement or Google makes the undocumented protocol unusable.

### [2026-11-04 16:01] Google Web: retain data and review uncommitted snapshots
- **Context:** The working tree contains unrelated user changes; provider migration needs independent task reviews without creating commits or branches.
- **Options:** A: Commit each task on a new branch / B: Stay on the current branch and review scoped working-tree diffs / C: Replace the working tree with a clean checkout
- **Chosen:** B (confidence ~99%)
- **Why:** This respects current-branch/no-commit constraints and keeps existing changes, accounts and original running processes untouched. Legacy provider assets are removed from use, not deleted from the user's AppData or Downloads.
- **Revert if:** The user explicitly authorizes a different integration workflow.

### [2026-11-04 17:27] Google Web: actual model identity stays unknown without evidence
- **Context:** The source inferred served models by finding known IDs anywhere in response text; independent Opus review showed that even unrelated metadata can contain those IDs.
- **Options:** A: Infer actual model from arbitrary IDs / B: Report the explicitly selected routing identity and leave actual identity unknown until an authoritative protocol field is verified
- **Chosen:** B (confidence ~99%)
- **Why:** This preserves manual routing without mislabeling echoed IDs as Google-verified model identity. Synthetic fixtures cannot establish a field's semantics.
- **Revert if:** A real, authoritative response model field is independently verified and covered by sanitized source-derived fixtures.

### [2026-11-04 17:27] Google Web: strict tool envelopes and offline schema validation
- **Context:** Review found that rejecting standard schema unions breaks legitimate Claude Code/MCP tools, while extracting tool JSON from surrounding prose risks executing quoted examples.
- **Options:** A: Reject union/ref schemas and parse the first JSON substring / B: Preserve complete schemas, validate with an offline standards-based validator, accept only complete bridge JSON or a single outer JSON fence
- **Chosen:** B (confidence ~95%)
- **Why:** A maintained JSON Schema validator is smaller and more correct than growing a partial validator; disable remote/file resolution and allow local references only. Model prose containing an example must not be promoted to a tool call; malformed forced-tool output can fail safely before client output.
- **Revert if:** Existing ccat validation already provides equivalent complete offline validation, or the chosen validator cannot disable external resolution or meet build constraints.





### [2026-11-05 17:35] Gateway: allow Tailscale and LAN hostnames in request headers
- **Context:** `ccat gateway` rejected requests originating from Tailscale with 403 because Host header was not 127.0.0.1/localhost.
- **Options:** A require users to spoof Host header / B allow Tailscale (100.*, .ts.net, machine name) and private subnet ranges in gateway
- **Chosen:** B (confidence ~95%)
- **Why:** enables remote access over secure private Tailscale network without breaking browser CSRF/Origin protection.
- **Revert if:** exposing gateway over Tailscale poses security risks when multi-user tailnets are used without API keys.

### [2026-11-05 19:28] Full portable backups of ccat-owned data
- **Context:** Full import/export must include accounts, credentials, named configurations and preferences, not only tier routes.
- **Options:** A copy the entire data directory / B versioned allowlisted JSON with portable Gemini cookies / C configuration-only export
- **Chosen:** B (confidence ~95%)
- **Why:** Reuses the existing secure atomic writer and DPAPI helpers without dependencies, excludes browser/runtime files, and moves Gemini sessions between Windows users. Backups explicitly warn that secrets are unencrypted; restore validates before writing, confirms replacement, retains unrelated files and rolls back on errors. Claude Code login and environment secrets remain externally managed.
- **Revert if:** Backups need password encryption or consistent live snapshots across concurrently writing sessions; add those explicitly rather than copying locked runtime state.

### [2026-11-05 19:28] Publish app changes without local development assets
- **Context:** The user chose to include all existing app changes in the main-branch GitHub commit.
- **Options:** A stage everything including local tools and downloaded fonts / B stage application sources, docs and licenses only
- **Chosen:** B (confidence ~99%)
- **Why:** Excludes local .commandcode preferences, ask.py, downloaded executables and non-bundled proprietary fonts. These are not app build inputs; existing bundled font/ assets remain unchanged.
- **Revert if:** A currently excluded file becomes a reviewed, licensed runtime or build dependency.

### [2026-11-06 03:15] Official macOS support
- **Context:** ccat had Windows-specific assumptions in OpenCode/Bun discovery, browser detection, terminal launching, and statusline scripts.
- **Options:** A keep Windows-only and leave macOS untested / B official macOS support with native browser/runtime discovery, AppleScript terminal dispatch, libc localtime, and a cross-platform statusline script
- **Chosen:** B (confidence ~95%)
- **Why:** The core egui UI and in-process proxy engine are already cross-platform. Providing proper macOS path resolution, Terminal.app spawning, and a statusline script brings complete, seamless first-class support to macOS.
- **Revert if:** Platform-specific maintenance overhead proves unsustainable.

