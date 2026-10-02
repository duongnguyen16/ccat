<p align="center"><img src="icon.png" alt="ccat" width="120"></p>

# ccat

**Claude Code, any model.** ccat is a small native launcher written in Rust: pick a provider and a folder, and it runs `claude` in your terminal against that provider. The main target is **Google Antigravity**: your Google accounts' Antigravity quota (Gemini, Claude, GPT-OSS) behind the Claude Code harness you already know. Any Anthropic-compatible endpoint works as well.

<p align="center">
  <img src="docs/launcher.png" alt="ccat launcher: provider, folder, bypass permissions, Launch" width="360">
  &nbsp;
</p>

```
ccat              # pick provider + folder, then Claude Code starts in this terminal
ccat --resume     # extra arguments go straight to claude
ccat --version    # the only flag ccat keeps for itself
```

ccat never edits your Claude config. Every override lives only in the child process's environment and flags, so a plain `claude` session stays exactly as it was.

## Why

- **All in one app.** No editing `settings.json`, no exporting `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN` / model variables by hand. URLs, keys, Google accounts and per-tier models live in profiles you set up once in the UI. Switching between them is one click before launch.
- **A proxy built in.** Antigravity is served by a proxy inside ccat itself. For this use case you don't need a separate router such as 9Router or OmniRoute to sit between Claude Code and the model.
- **Claude Code on Antigravity.** Antigravity's quota is generous, but its agent isn't Claude Code. ccat gives you both: Claude Code's skills, hooks, plugins, subagents, MCP servers and permission model, running on Antigravity models, plus the Antigravity abilities Claude Code has no tool for.
- **Small and fast.** One native Rust binary: egui UI, plain blocking HTTP, no Node, no Python, no Docker, no background service. The proxy is a thin translation layer that only runs while your Claude Code session does.

## ccat vs Antigravity

What Antigravity (the IDE / `agy` CLI) offers, and where ccat stands on each:

| Capability                            |      Antigravity      |  ccat (Claude Code)  | Notes                                                                   |
| ------------------------------------- | :-------------------: | :------------------: | ----------------------------------------------------------------------- |
| Gemini models                         |          ✅           |          ✅          | Served by ccat's local proxy                                            |
| Claude / GPT-OSS on Antigravity quota |          ✅           |          ✅          | Same Cloud Code backend, same quota buckets                             |
| Agent harness                         |   Antigravity agent   |    ✅ Claude Code    | Skills, hooks, plugins, subagents, MCP, plan mode, `/context`           |
| Web search (Google Search)            |          ✅           |          ✅          | Google Search grounding; see [Special features](#special-features)      |
| Image generation / editing            |  ✅ `generate_image`  |          ✅          | Bundled MCP tool `mcp__antigravity__generate_image`                     |
| Several Google accounts               | One signed-in account |          ✅          | Pool with automatic rotation when one runs out                          |
| Quota in the status line              |      ✅ built-in      |          ✅          | `statusline/` script, next to your Claude plan limits                   |
| Third-party providers                 |      Google only      |          ✅          | Any Anthropic-compatible endpoint (OpenRouter, gateways, local servers) |
| Exact token counts                    |          ✅           | ✅ Gemini / ≈ others | Google's counter on Gemini routes; a calibrated estimate elsewhere      |
| Browser agent                         |          ✅           |          ❌          | Not bridged. Use a browser MCP server in Claude Code instead            |
| Knowledge / "brain" memory            |          ✅           |          ➖          | Claude Code has its own memory (`CLAUDE.md`, auto memory)               |
| Voice input, remote control           |          ✅           |          ❌          | Not bridged                                                             |

✅ supported · ➖ covered differently by Claude Code · ❌ not supported

## Special features

- **Antigravity Web Search.** Claude Code's `WebSearch` runs as Google Search grounding, and the results come back as Anthropic web-search blocks, so citations and follow-up `WebFetch` work as usual. Grounding only runs on Gemini, so a search asked of a Claude or GPT route is delegated to Gemini Flash and the result goes back to the model you picked.
- **Image generation and editing.** ccat registers an `antigravity` MCP server with Claude Code. Its `generate_image` tool uses Gemini's image model and can take existing images as references. Files are saved relative to your project folder.
- **Account pool with failover.** Add as many Google accounts as you like. Requests stick to one account and move to the next when Google reports quota exhaustion. Quota is tracked per model family (Gemini, Claude, GPT), so an account out of Claude quota still serves Gemini. Retry delays are read from Google's error.
- **Claude names for any model.** Each tier (Default, Fable, Opus, Sonnet, Haiku) maps to any model, and can optionally be presented to Claude Code under a Claude model name. Claude Code then applies that model's catalog entry (context window, thinking), while the request still goes to the real model.
- **Faithful translation.** Streaming and non-streaming Messages ↔ Cloud Code `streamGenerateContent`, thinking blocks and their signatures (safe across model switches and resumed sessions), and tool schemas normalized to what every Antigravity model family accepts.
- **Quota status line.** One PowerShell script feeds both status lines: in `agy` it records the quota Antigravity hands it, and in Claude Code it shows that quota next to context use and your Claude plan limits, as smooth bars:

  ```
  ◔ ▌░░░░░░░ 7% │ ✻ Claude 5h █▉░░░░░░ 24% 7d ████▍░░░ 55% │ ▲ ✦ Gemini 5h ▏░░░░░░░ 2% 7d ▎░░░░░░░ 3%
  ```

  The Claude/GPT bucket (`◇`) appears only when a ccat Antigravity provider actually maps a non-Gemini model.

- **Stays current.** The backend gates newer models on the client version, so ccat reports the live Antigravity release version, and the list of Claude model names to present as comes from [models.dev](https://models.dev) on every start.

## Providers

| Provider                                   | What it does                                                                                                                                                                                                      |
| ------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Default**                                | No overrides. Plain `claude` with your normal login.                                                                                                                                                              |
| **Google Antigravity**                     | Sign in with one or more Google accounts in the browser. ccat provisions the Cloud Code project, lists the models your accounts can use, and serves them through a local proxy while Claude Code runs.            |
| **Anthropic-compatible endpoint** (custom) | Any server that speaks the Anthropic Messages API: OpenRouter, an API gateway, a local model server. Set a URL and key, **Fetch model list** (Anthropic- or OpenAI-style `/v1/models`), or type custom model ids. |

Providers are profiles: add, clone, rename and delete them in **Config**, then pick one in the launcher. **Bypass permissions** adds `--dangerously-skip-permissions`. Next to Config, the GitHub button opens this repo, and the version label turns into `vX → vY` when a newer release is out (click it to open the release).

## Install

Needs [Rust](https://rustup.rs) and [Claude Code](https://docs.claude.com/en/docs/claude-code) on your `PATH`.

```bash
git clone https://github.com/duongnguyen16/ccat
cd ccat
cargo install --path .
```

Developed and used on Windows. The UI (egui) and the proxy are cross-platform, but macOS and Linux are untested.

### Status line (optional, Windows PowerShell)

Claude Code, in `~/.claude/settings.json`:

```json
"statusLine": {
  "type": "command",
  "command": "powershell -NoProfile -ExecutionPolicy Bypass -File C:/path/to/ccat/statusline/quota-statusline.ps1"
}
```

Antigravity CLI, in `~/.gemini/antigravity-cli/settings.json` (or run `/statusline <command>` inside `agy`):

```json
"statusLine": {
  "enabled": true,
  "command": "powershell -NoProfile -ExecutionPolicy Bypass -File C:\\path\\to\\ccat\\statusline\\quota-statusline.ps1 -Agy"
}
```

The Antigravity quota refreshes whenever `agy` is open. A dim `~Nh` marks a reading that is N hours old. Bars turn yellow at 50% and red at 80%.

## How it works

```
claude ──► ccat proxy (127.0.0.1, random port, per-launch token)
             ├─ POST /v1/messages ──► Cloud Code streamGenerateContent ──► Anthropic SSE back
             ├─ count_tokens      ──► Google countTokens (Gemini) / calibrated estimate
             └─ /mcp              ──► generate_image (Gemini image model)
```

- The proxy binds to `127.0.0.1` only, on a random port, and answers only requests that carry the token generated for that launch.
- Config, including the Google refresh tokens, is stored in `%APPDATA%\ccat\config.json` (`$XDG_CONFIG_HOME/ccat` or `~/.config/ccat` elsewhere). Keep that file private.
- Proxy logs go to `antigravity.log` next to the config, since Claude Code owns the terminal while ccat runs.

## Notes

- Antigravity's Cloud Code API is internal and undocumented. Request shapes follow oh-my-pi's `google-antigravity` provider and may break when Google changes them.
- Use your own accounts, within Google's terms for Antigravity.

## License

[MIT](LICENSE). The bundled fonts (Inter, JetBrains Mono, Newsreader) are under the [SIL Open Font License 1.1](font/OFL.txt).
