# OpenCode provider and egress rebuild

## Intent
Replace the failed OpenCode Free request-emulation layer with an honest Anthropic-first integration inside ccat. The special provider remains anonymous and URL-configurable. Its settings show outbound pools, configured local IPs/proxies, health and connections; never upstream/provider server IP addresses.

## Verified baseline
- Official OpenCode inference documentation restricts free chat models to OpenCode itself; third-party access can be refused even with a key. Egress pools do not grant access or increase quota.
- The current adapter invents CLI identities, injects an unrelated harness and tools, and rewrites the model's tool calls. Remove that behavior, not tune its fingerprints.
- Existing ccat already supplies the authenticated local Anthropic gateway, tier aliases, fallback chains, OpenAI conversion, Responses conversion and Anthropic SSE. Reuse these, correcting verified defects only.
- The current free catalog exposes Chat Completions models. Preserve an explicit protocol mapping for Anthropic and Responses rather than pretending every model supports every wire format.

## Decisions (builder defaults)
- Stay in the current branch and existing binary. No standalone gateway, no external runtime/Bun dependency, no commits without request.
- Reuse ccat JSON configuration rather than introducing TOML. Each OpenCode provider stores an egress configuration; missing settings mean one enabled Direct node.
- Add reqwest blocking with Rust TLS and SOCKS support for this provider only: ureq remains for unaffected providers. Each node owns one reusable client.
- Node kinds: Direct, local-IP binding, HTTP proxy, HTTPS proxy, SOCKS5 (remote DNS). Proxy credentials are environment-variable references, never plaintext URLs/passwords.
- Weighted sticky rendezvous routing uses the authenticated gateway scope plus conversation ID, not subagent ID. Keep sessions on their selected healthy node.
- At most one alternative node on a proven connection establishment failure, before a request could be processed. Never fail over on HTTP 401/403/429, body/read failures, ambiguous timeouts or after output starts. No automatic paid escalation.
- Disabled/draining nodes admit no new streams; existing streams finish. Concurrency is bounded; locks never cover network I/O.
- Transport health is distinct from upstream access/quota. An HTTP response, including 403/429, proves connectivity, not inference permission. Background checks use the public model endpoint, not inference prompts.
- Persist small credential-free per-process metric snapshots for the Settings process: node ID/state, configured address, latency/TTFT, active streams, errors, last check. No provider IP lookup or public-IP echo service is contacted.
- Anonymous requests carry only the legitimate anonymous marker if needed; do not fabricate OpenCode client headers or system prompts. Inbound local gateway credentials never travel upstream.
- Preserve caller tool names, schemas, argument names and correlation IDs. No manifest injection or tool renaming.
- Native Anthropic passes through protocol fields and valid signatures. Chat/Responses conversions explicitly reject unsupported semantics with recoverable capability errors rather than silently dropping them. No fake thinking signatures.
- Token counting is optional: unsupported exact counting returns an unsupported-route response so Claude Code estimates locally; never present character estimates as exact counts. Support HEAD /api/hello.
- OpenCode admission/quota errors are visible immediately after configured fallbacks; no invisible multi-hour free-provider hold. Other providers' wait behavior remains unchanged. Upstream error categories and retry hints are preserved safely; raw diagnostic bodies are not written to normal logs.

## Settings
Use the existing Settings design and controls. OpenCode Free has its own page with URL and the pool list, Add/Remove, enable/drain, type, IP/proxy endpoint, weight and optional environment secret names. Display honest Idle/Healthy/Degraded/Unhealthy/Draining/Disabled state and measured connection counts/latencies. No fake health, no provider-side IPs, no mandatory API-key field.

## Acceptance
Mock tests cover local auth isolation, stable session routing with independent subagents, tool/result correlation, text/tool streaming completion, cancellation, native Anthropic and Responses branches, safe errors, invalid proxy config, bind-IP and proxy routing, pre-connect-only failover, no rotation on 403/429 and graceful draining. Run the complete existing suite, build release and install locally. Attempt Linux verification when a toolchain is available; explicitly report unavailable platforms. Live inference tests are bounded and opt-in; mocks do not establish upstream free-tier permission.

## Sources
- https://code.claude.com/docs/en/llm-gateway-protocol
- https://opencode.ai/v2/docs/console/api/inference
- https://docs.rs/reqwest/latest/reqwest/
