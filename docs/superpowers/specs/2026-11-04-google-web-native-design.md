# Native Google Web provider

## Intent

Implement Gemini Web directly in the Rust ccat application using the supplied Argon Local source as the protocol reference. This supersedes the copied Go/Python bridge implementation and its incorrect completion claims. “Official ccat provider” means a first-class ccat provider, not a claim that the undocumented Google Web protocol is an official Google API.

## Requirements

- No runtime dependency on the Argon Downloads directory, gemini-api.exe, Python, or Playwright. An installed supported browser is allowed for the user's interactive Google sign-in.
- No provider URL, API key, bridge ports, or Start/Stop-server controls in the provider UI. Routing uses the enabled account pool.
- Preserve existing AppData accounts, DPAPI credentials, Chrome profiles, preference/rate settings and available history; leave the original source folder and running original processes untouched.
- Native authentication/session refresh, Gemini batchexecute requests, actual model-selection headers, response/error parsing, tools, attachments and source media/research capabilities. Model choice remains explicitly user-selected; account metadata is not permission to silently substitute another model.
- Preserve account enable/preference, round-robin/least-used scheduling, concurrency/RPM limits, FIFO queueing, per-account history/latency/errors, checks/playground and cookie-free import/export.
- Never send Claude/gateway credentials upstream. Google cookies only reach trusted Google endpoints. Validate IDs, profile paths, file sizes, imports and generated output paths. Keep cookie values and prompts out of normal logs/history.
- Login, refresh, checks, generation and imports run off the egui thread. Distinguish initializing, ready, signed-out, cooldown and unknown states.
- Usage must report verified numbers/reset data only. Unknown Google quota is explicitly unknown; connection state is never converted into 0%/100% usage.
- Recover authentication/transient/pre-content errors inside the provider before committing client headers. Do not replay after content was sent. Disabling/removing an account immediately prevents new use.
- Keep compatibility with the stored `gemini_web` provider flag/name while presenting a native Google Web experience. Migrate away from the obsolete stored URL/key, without altering unrelated providers/routes.
- Preserve MIT attribution for the ported Argon bridge source. No new branch, commits, merge, or user-configuration changes unrelated to the provider.

## Implementation approach (builder choices)

Use the existing Rust dependencies: ureq 3.4.2 for blocking HTTPS on worker threads, serde_json for protocol arrays, tungstenite 0.26.2 for native Chrome DevTools sign-in, Windows DPAPI for existing secrets, and existing ccat proxy/SSE/gateway/analytics helpers. Port the source's model-routing metadata, 81-slot generation payload, uploads and parser rather than rebuilding an HTTP sidecar.

Separate focused implementation files under `src/providers/gemini_web/`: `wire.rs` (Google protocol), `store.rs` (private account/config/runtime persistence), `browser.rs` (owned browser sign-in), `messages.rs` (Anthropic conversation/tools), `media.rs` (source media/research operations), with `gemini_web.rs` owning the account pool, UI and proxy entry points. Reuse existing helpers before adding abstractions.

## Acceptance

Existing tests plus new deterministic protocol/store/conversion/recovery checks must pass. Verify the built release no longer launches or routes through the Go/Python assets. Test login/session initialization and a minimal real Google request when the existing session permits it; report any authentication step requiring the user's interaction instead of claiming it passed. Build and install the release on the current machine without killing live ccat sessions.

Tích hợp Google Web sẽ chạy trực tiếp bên trong ccat, giữ dữ liệu tài khoản và các tính năng của bản Argon gốc. Giao diện không yêu cầu URL hay API key; trạng thái đăng nhập và quota sẽ được hiển thị trung thực. Việc kiểm tra cuối cùng phải phân biệt rõ những gì đã chạy thành công với bước còn cần người dùng đăng nhập.
