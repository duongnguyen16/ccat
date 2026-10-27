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
