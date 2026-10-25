# Claude Code token audit - 2026-10-24

Source: the pasted MuseAI request for “What is ur model” (566,092 file bytes; 564,738 characters after newline normalization).

**Token figures below are rough character-count / 4 estimates, not tokenizer measurements or billed usage. Native MuseAI does not report upstream token counts. Official Claude cache-read billing also differs from fresh input billing.**

## Main finding

`C:\Users\duncuti16\.claude.json` had `env.ENABLE_TOOL_SEARCH` set to `false`. This forces MCP definitions into context instead of loading them on demand. Applied: moved this control to the standard `C:\Users\duncuti16\.claude\settings.json` with `env.ENABLE_TOOL_SEARCH="true"` and removed the legacy false entry. All integrations remain discoverable. Both files were backed up privately, and a read-only comparison confirmed all unrelated Claude settings were preserved.

| Pasted request component | Characters | Share | Rough tokens |
|---|---:|---:|---:|
| Tool catalog (245 definitions) | 493,213 | 87.3% | ~123,304 |
| Conversation and injected instructions | 71,096 | 12.6% | ~17,774 |
| ccat adapter wrapper and framing | 429 | 0.1% | ~108 |

The user question is 16 characters. The pasted exchange contains no client tool calls: all 245 definitions were unused in that exchange. That does not make those tools unnecessary for future coding work.

## Controlled local capture: same prompt, same Claude account, different tool-search setting

| Tool search | Tool definitions sent upfront | Tool JSON characters | Total request JSON characters |
|---|---:|---:|---:|
| disabled | 127 | 264,603 | 323,831 |
| enabled | 17 | 43,701 | 110,931 |

Tool JSON reduction: 83.5%. Overall request reduction: 65.7%. These are serialized-request sizes; server-side lazy loading can save additional model context. The control uses a print-mode greeting, so its built-in tool count differs from the pasted interactive session.

## All tool groups, largest first

| Group | Tools | Compact JSON characters | Rough tokens |
|---|---:|---:|---:|
| `Claude built-ins` | 41 | 165,770 | ~41,443 |
| `mcp__claude_ai_Figma__` | 43 | 97,449 | ~24,363 |
| `mcp__claude_ai_Gmail__` | 30 | 70,452 | ~17,613 |
| `mcp__claude_ai_Atlassian__` | 41 | 59,493 | ~14,874 |
| `mcp__claude-in-chrome__` | 22 | 28,939 | ~7,235 |
| `mcp__claude_ai_Google_Calendar__` | 9 | 25,999 | ~6,500 |
| `mcp__claude_ai_Claude_Docs__` | 8 | 4,991 | ~1,248 |
| `mcp__context7__` | 2 | 4,972 | ~1,243 |
| `mcp__plugin_engineering_atlassian__` | 2 | 1,474 | ~369 |
| `mcp__claude_ai_Microsoft_365__` | 2 | 1,469 | ~368 |
| `mcp__claude_ai_Google_Sheets__` | 2 | 1,466 | ~367 |
| `mcp__plugin_engineering_datadog__` | 2 | 1,464 | ~366 |
| `mcp__claude_ai_HeyJapan_MCP__` | 2 | 1,461 | ~366 |
| `mcp__claude_ai_Google_Drive__` | 2 | 1,460 | ~365 |
| `mcp__plugin_engineering_linear__` | 2 | 1,456 | ~364 |
| `mcp__plugin_engineering_notion__` | 2 | 1,456 | ~364 |
| `mcp__claude_ai_Google_Docs__` | 2 | 1,454 | ~364 |
| `mcp__plugin_context7_context7__` | 2 | 1,453 | ~364 |
| `mcp__plugin_engineering_slack__` | 2 | 1,450 | ~363 |
| `mcp__claude_ai_ElevenLabs__` | 2 | 1,444 | ~361 |
| `mcp__claude_ai_monday_com__` | 2 | 1,441 | ~361 |
| `mcp__claude_ai_Intercom__` | 2 | 1,433 | ~359 |
| `mcp__claude_ai_eup_chat__` | 2 | 1,428 | ~357 |
| `mcp__claude_ai_HubSpot__` | 2 | 1,427 | ~357 |
| `mcp__claude_ai_Linear__` | 2 | 1,421 | ~356 |
| `mcp__claude_ai_Mobbin__` | 2 | 1,421 | ~356 |
| `mcp__claude_ai_Notion__` | 2 | 1,421 | ~356 |
| `mcp__claude_ai_Asana__` | 2 | 1,415 | ~354 |
| `mcp__claude_ai_Canva__` | 2 | 1,415 | ~354 |
| `mcp__claude_ai_Box__` | 2 | 1,403 | ~351 |
| `mcp__claude_ai_Exa__` | 2 | 1,402 | ~351 |
| `mcp__antigravity__` | 1 | 992 | ~248 |
| `mcp__web-search__` | 2 | 776 | ~194 |

## Largest individual tools

| Tool | Characters | Description characters | Schema characters |
|---|---:|---:|---:|
| `Artifact` | 37,406 | 21,621 | 15,620 |
| `ArtifactData` | 12,241 | 4,168 | 7,969 |
| `DesignSync` | 9,340 | 4,132 | 5,124 |
| `mcp__claude_ai_Google_Calendar__create_event` | 9,088 | 39 | 8,961 |
| `mcp__claude_ai_Gmail__search_threads` | 8,837 | 2,375 | 6,367 |
| `PowerShell` | 8,629 | 7,754 | 712 |
| `Monitor` | 7,796 | 6,581 | 1,042 |
| `ArtifactComments` | 7,438 | 5,141 | 2,203 |
| `mcp__claude_ai_Google_Calendar__update_event` | 7,284 | 39 | 7,157 |
| `PublishPlugin` | 6,970 | 2,296 | 4,607 |
| `mcp__claude_ai_Atlassian__createConfluenceInlineComment` | 6,064 | 472 | 5,493 |
| `mcp__claude-in-chrome__computer` | 6,029 | 678 | 5,273 |
| `SendMessage` | 5,677 | 4,259 | 1,291 |
| `mcp__claude_ai_Gmail__list_drafts` | 5,611 | 182 | 5,349 |
| `Workflow` | 5,355 | 3,480 | 1,775 |
| `mcp__claude_ai_Gmail__send_message` | 5,342 | 853 | 4,406 |
| `mcp__claude_ai_Gmail__create_label` | 5,115 | 243 | 4,791 |
| `mcp__claude_ai_Gmail__reply` | 4,989 | 916 | 3,997 |
| `mcp__claude_ai_Figma__get_design_context` | 4,898 | 1,588 | 3,220 |
| `AskUserQuestion` | 4,897 | 1,786 | 3,023 |
| `mcp__claude_ai_Gmail__update_draft` | 4,859 | 795 | 3,983 |
| `mcp__claude_ai_Atlassian__createConfluenceFooterComment` | 4,767 | 46 | 4,622 |
| `Agent` | 4,725 | 3,049 | 1,600 |
| `mcp__claude_ai_Gmail__update_label` | 4,665 | 95 | 4,491 |
| `ScheduleWakeup` | 4,631 | 3,148 | 1,392 |

**Complete inventory:** [all 245 tools](claude-tool-token-inventory.csv). Each row includes name, group, size, estimate, description/schema sizes and whether it was used in this exchange.

## Injected conversation context

| Message | Role | Content characters | Identification |
|---|---|---:|---|
| 0 | system | 13,892 | Claude harness, memory/context rules and browser guidance |
| 1 | user | 5,212 | Global user instructions, RTK and user reminder |
| 2 | system | 46,087 | Startup hook output: Ponytail, skill/agent catalog, MCP server instructions, Advisor |
| 3 | assistant | 139 | Previous assistant greeting |
| 4 | user | 16 | User question |
| 5 | system | 4,649 | Additional MCP server instructions |

The startup hook alone contains approximately 31,281 characters of MCP server instructions, 6,502 characters in the Ponytail/skill-guidance section, 6,141 characters in its environment/catalog section and 2,092 characters of Advisor guidance. The primary system message also contains 3,353 characters of Chrome guidance. Some of this is irrelevant to an identity question but carries user preferences, permissions or tool-specific safety rules; it was not blindly deleted.

## Actions and remaining candidates

- Enabled global tool search so official Claude and proxy-backed sessions load tools on demand. Connector registrations and credentials are preserved.
- Updated ccat's MuseAI, ChatGPT, Antigravity and Ollama adapters to resolve dynamically loaded `tool_definition` metadata, send each declaration once, and omit deferred tools until selected. Real schemas, tool history, explicit tool choices and argument validation are preserved. Official forwarding keeps the original request unchanged.
- Keep the current browser, plugins, global CLAUDE.md and safety instructions. Browser startup (22 tool definitions plus Chrome guidance), duplicate Context7 plugin/standalone registration and always-on startup catalog are optional follow-up reductions, not safe deletions inferred from one short question.
- MuseAI history polling has no demonstrated upstream prompt cache: ccat sends the complete current context in a fresh isolated request session. Enabling lazy tool loading reduces what must be sent; it does not invent cached-token billing.
- Native MuseAI currently emits no upstream token usage. Zero usage in its response must not be interpreted as zero tokens consumed. Accurate billed usage needs provider telemetry.

## References

- [Claude Code MCP tool search](https://code.claude.com/docs/en/mcp#configure-tool-search)
- [Claude Code CLI tool filtering](https://code.claude.com/docs/en/cli-reference)

## Verification and rollout

122 offline tests passed, including tool discovery and tool-result references across all four translated providers. A real Claude Team → MuseAI → client ToolSearch → final-answer round trip passed. The release build succeeded, and the CLI and Downloads binaries were updated with matching hashes. Local capture with a scoped `true` setting verified the loading behavior before migration; the saved global setting was checked without making another official inference request. Exact billed-token savings were not measured.

Open a fresh Claude/ccat session to use the new default and adapter. Existing sessions were left running. User instructions, safety rules, plugins, browser registration and accounts were preserved.
