# OpenCode proxy source audit

## References actually inspected

- `12errh/zen-proxy`: `zen-proxy.mjs` upstream/auth/header/body/probe/SSE paths, README, captured preamble and tools.
- `ebouther/opencode-unlimited-proxy`: README, `stack/config.json`, `stack/docker-compose.yml`, `docs/free-tier-gates.md`, and the legacy `stack/warp/rotator.py` implementation.
- Sources were downloaded into separate temporary directories and inspected as data. No downloaded scripts, installers, containers or probes were executed.

## Confirmed pre-fix defects in ccat

1. **Anonymous authorization missing.** `openai::messages` gave the free path an empty key, so `openai::open` omitted Authorization. The reference supplies the non-secret `Bearer public` marker. No user API key is needed, but this is not equivalent to dropping the protocol marker.
2. **Different HTTP transport.** The reference calls Bun's `fetch` and explicitly warns about refusal from other HTTP stacks. ccat used a fresh ureq client. This is a real implementation mismatch, not evidence that a device is out of quota.
3. **Pool configuration was inert.** `Provider.opencode_egress` reached the settings form only. `target()` did not carry it into `Upstream`, and the request sender did not read it. Selecting/disabling nodes had no effect on networking.
4. **Refusal hidden by unlimited-looking wait.** A free-only route returning 429 entered the generic multi-hour quota hold. Reference source documents that 429 can also encode client/payload restrictions, so this was neither a useful user experience nor a sound device-quota diagnosis.
5. **Parallel call fragment bug.** `opencode::Calls` joined an argument delta only to the last held call. Interleaved indices could split one tool call into multiple calls and lose its arguments/ID.
6. **Live probe differed from the reference.** ccat's probe requested a 1,000-token budget and previously treated a printed error as a passing test. The reference uses a full 32,000-token agent-shaped probe. A refusal cannot be counted as a successful live verification.

## Things that were NOT the problem established by this audit

- The bundled preamble matches the reference snapshot.
- Each of ccat's six mapped tool schemas is structurally identical to the same tool in the current reference capture. The reference file includes additional unmapped tools, which its loader filters out.
- An HTTP 429 alone does not establish exhausted device quota. The user's working native OpenCode session remains a valid counterexample to a blanket quota diagnosis.

## What the second repository really runs

Its current Compose stack pulls `ghcr.io/jasonxu114514/opencode2api:v1.4.5`; it is not a reusable Rust implementation of the requested ccat provider. The wrapper config enables anonymous/free-only mode and a userspace WARP pool. Its documented policy retires/re-registers exits on 403/429 to obtain another allowance. That behavior conflicts with the user's earlier explicit no-quota/admission-bypass requirement and is not adopted. No WARP installation, terms acceptance, public-IP echo service or GPL engine code is introduced into ccat.

## Repair and verification contract

- Route the saved egress configuration into the actual request sender.
- Use a bounded, credential-isolated Bun worker transport, with reusable idle workers and cancellation cleanup; preserve anonymous public auth rather than forwarding the local client token.
- Keep session affinity scoped by the authenticated local principal and conversation, not subagent identity.
- Do not switch IPs for 403/429 or ambiguous delivery failures.
- Return a free-only refusal to Claude Code instead of silently holding it; leave other providers' configured wait policy alone.
- Make pool configuration editable, show configured addresses only, and never label unmeasured status as healthy.
- Direct and HTTP(S) proxy routes are supported by Bun fetch. Source-IP binding and SOCKS5 must be rejected explicitly until a verified implementation exists, never silently treated as Direct.
- Test actual local proxy traversal, anonymous auth, incremental SSE, clean worker reuse, cancellation, parallel tool correlation, admission errors and config propagation.
- A real successful model reply is required before claiming live inference works. Local/mock test success does not prove upstream access; no unlimited-access claim is made.

## Source links

- https://github.com/12errh/zen-proxy/blob/main/zen-proxy.mjs
- https://github.com/ebouther/opencode-unlimited-proxy/blob/main/stack/config.json
- https://github.com/ebouther/opencode-unlimited-proxy/blob/main/stack/docker-compose.yml
- https://github.com/ebouther/opencode-unlimited-proxy/blob/main/docs/free-tier-gates.md
- https://opencode.ai/v2/docs/console/api/inference
