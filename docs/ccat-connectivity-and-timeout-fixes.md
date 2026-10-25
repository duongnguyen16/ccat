# Connectivity, long-session errors and token improvements

Verified 2026-10-24, Asia/Bangkok. Repository: `D:\eUp\ccat`.

## Current connection refusal

The canonical Claude user settings contained `ANTHROPIC_BASE_URL=http://127.0.0.1:8080`, with no listening proxy at that address. It was absent from the earlier settings backup; its origin is unconfirmed. The dead override was backed up and removed. ccat now passes its selected proxy URL through Claude's command-line settings, which take precedence over saved user/project settings. Credentials remain in the child environment, outside command-line settings.

A real Claude Code → ccat → native MuseAI ToolSearch round trip passed with a deliberately dead project-level endpoint. Existing processes keep their launch settings: relaunch ccat to load the fix, then resume the conversation if needed.

## Yesterday: October 23

The audit includes intermediate `system/api_error` retries and final assistant failures from 35 candidate top-level transcripts. Counts are log events, not unique requests or charges.

| Evidence | Finding |
|---|---|
| 12 Claude API error events | 10 connection resets/stream failures, 2 timeouts |
| 03:53:31, dun-core session | Final request timeout; insufficient matching ccat evidence to establish its cause |
| 14:53:45–15:02:27, eUp session | Repeated ECONNRESET, one timeout at 14:55:33, then a final reset after ten retries |
| ccat at 14:59:50 | ChatGPT backend connection timeout and a 502; account skipped for 30 seconds |
| 2,263 quota-related ccat log entries | Quota pressure and waits; repeated entries are not independent failures |
| Authentication | No confirmed expired-authentication 401 in this audit |

The evidence supports upstream connectivity failures and quota pressure. It does not establish an expiring ccat session or a single fixed lifetime limit. See [sanitized event evidence](claude-long-session-error-audit.json).

## Transport fixes

- Native Muse socket polling timeouts now continue reading until the request deadline, without resending a request. Cancellation remains active.
- The native reply budget is bounded at 600 seconds; cancellation and deadline errors have distinct messages/statuses.
- Google and OpenAI-compatible upstream keepalive comments/pings now reach Claude as Anthropic ping events, including before content arrives.
- Stream failures identify the provider. Non-secret request diagnostics record sizes/tool counts, and native Muse records request ID, duration and status. Native billed-token usage is explicitly unavailable.

Different clocks serve different purposes: the native socket polling interval is about three seconds, gateway handshake/unary operations have 30-second deadlines, native reply completion has a 600-second budget, and Claude's enabled stream watchdog has a separate idle limit. The historical legacy API timeout setting was 3,000,000 ms (50 minutes); increasing that alone would not fix connectivity or missing keepalives. The remote Muse bridge retains its separate 120-second HTTP budget.

## Token improvements applied

Global tool search and native PowerShell remain enabled. The existing native `rtk hook claude` matcher now covers `Bash|PowerShell`. Nine self-checks covered supported/unsupported commands, Unicode, quoted paths, pipelines and failures; failing Git/Cargo exit codes and key diagnostics survived. The hook returns updated command input without granting permission. See [self-check evidence](powershell-rtk-selfcheck.json).

The user rule `C:\Users\duncuti16\.claude\rules\token-efficiency.md` directs targeted retrieval, evidence reuse, bounded worker context, stable caching and useful compaction ledgers. Restricted reviewers retain their permissions; the parent retrieves unavailable documentation when needed. Models, effort, Advisor, browser integrations and security hooks were preserved. Vendor plugin caches were not patched. See the [updated plan](claude-token-saving-plan.md).

## Verification and recovery

- `cargo test`: 125 passed, 8 live/explicit-opt-in tests ignored, zero failures.
- Explicit live Claude/Muse ToolSearch regression: passed, including poisoned project endpoint.
- `cargo build --release`: passed.
- Socket polling and upstream keepalive regressions cover the transport changes.
- Settings and executable backups: `C:\Users\duncuti16\AppData\Local\ccat-recovery\20261024-154436`.

No overnight soak test was run. The audit does not cover every subagent transcript. Payload reductions are measured characters, not billed-token savings; native Muse quota still comes from its existing cache.
