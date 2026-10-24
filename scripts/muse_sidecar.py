#!/usr/bin/env python3
"""
MuseAI Sidecar for ccat: Bridges OpenAI Chat Completions to Meta Hatch VM Gateway via Noise WebSocket.
Listens on http://127.0.0.1:18615 by default.
"""

import sys
import os
import json
import time
import hmac
import argparse
import threading
import urllib.parse
from http.server import HTTPServer, BaseHTTPRequestHandler

# Import muse_cli from site-packages or local
try:
    from muse_cli.cli import connect, load_config, fmt_event, is_reply
    from muse_cli.gateway import GatewayError
except ImportError:
    # If running directly without active venv, search app-owned venv
    appdata = os.environ.get("APPDATA") or os.path.expanduser("~/.config")
    venv_site = os.path.join(appdata, "ccat", "muse-venv", "Lib", "site-packages")
    if os.path.exists(venv_site) and venv_site not in sys.path:
        sys.path.insert(0, venv_site)
    from muse_cli.cli import connect, load_config, fmt_event, is_reply
    from muse_cli.gateway import GatewayError

POLL = 0.5


class MuseBridge:
    def __init__(self):
        self.lock = threading.Lock()
        self.cfg = load_config()
        self._gw = None
        self._last_cookies_mtime = self._get_cookies_mtime()

    def _get_cookies_mtime(self):
        cookies_file = self.cfg.get("cookies_file")
        if cookies_file and os.path.exists(cookies_file):
            try:
                return os.path.getmtime(cookies_file)
            except Exception:
                pass
        return 0

    def get_gateway(self):
        with self.lock:
            current_mtime = self._get_cookies_mtime()
            if current_mtime != self._last_cookies_mtime:
                self.reset_gateway()
                self.cfg = load_config()
                self._last_cookies_mtime = current_mtime

            if self._gw is None:
                self._gw = connect(self.cfg)
            return self._gw

    def reset_gateway(self):
        with self.lock:
            if self._gw:
                try:
                    self._gw.close()
                except Exception:
                    pass
                self._gw = None
            self.cfg = load_config()
            self._last_cookies_mtime = self._get_cookies_mtime()

    def execute_chat(self, prompt, session_title="ccat-turn", timeout=60):
        """Execute a prompt in an isolated session, yielding intermediate text deltas and final text."""
        gw = self.get_gateway()
        sess = None
        sid = None
        try:
            sess = gw.call_json('session.start', body={
                'method': '/api/session/start',
                'params': {'origin': 'fresh', 'lifecycle': 'persistent', 'title': session_title}
            })
            sid = sess.get('session_id')
            scope = {'session_id': sid}

            h = gw.call_json('chat.history', body={'limit': 1, **scope})
            baseline = max([e.get('seq', 0) for e in h.get('chat_events', [])], default=0)

            gw._open('chat.stream', body={
                'items': [{'type': 'text', 'text': prompt}],
                'node_id': self.cfg['node_id'],
                'capabilities': {},
                **scope
            })

            last_len = 0
            deadline = time.time() + timeout
            final_text = None

            while time.time() < deadline:
                try:
                    h = gw.call_json('chat.history', body={'limit': 10, **scope})
                except Exception:
                    time.sleep(POLL)
                    continue

                for ev in sorted(h.get('chat_events', []), key=lambda e: e.get('seq', 0)):
                    if ev.get('event_name') == 'message.assistant' and (ev.get('seq') or 0) > baseline:
                        p = ev.get('payload', {}) if isinstance(ev.get('payload'), dict) else {}
                        text = p.get('display_text') or p.get('content') or ''
                        if len(text) > last_len:
                            chunk = text[last_len:]
                            last_len = len(text)
                            yield ('delta', chunk)
                        if p.get('status') == 'completed' or p.get('status'):
                            final_text = text
                            break
                if final_text is not None:
                    break
                time.sleep(POLL)

            yield ('done', final_text or '')

        except Exception as e:
            self.reset_gateway()
            raise e
        finally:
            if sid:
                try:
                    gw.call_json('session.delete', body={'method': '/api/session/delete', 'session_id': sid})
                except Exception:
                    pass


BRIDGE = MuseBridge()


def format_tools_prompt(tools):
    lines = [
        "[AVAILABLE CLIENT TOOLS]",
        "You have access to the following client-side tools:",
    ]
    for t in tools:
        fn = t.get("function", t)
        name = fn.get("name", "")
        desc = fn.get("description", "")
        params = json.dumps(fn.get("parameters", fn.get("input_schema", {})))
        lines.append(f"- Tool: {name}\n  Description: {desc}\n  Parameters: {params}")

    lines.extend([
        "",
        "CRITICAL INSTRUCTIONS FOR TOOL EXECUTION:",
        "1. You are running as an external model backend. You have NO filesystem access, terminal, or tools on this server.",
        "2. Do NOT attempt to run internal commands or search the local server filesystem.",
        "3. When you need to read, write, edit files, or execute bash commands, respond with ONLY a JSON block in this exact format:",
        "```json",
        '{"tool_calls": [{"name": "<tool_name>", "arguments": { ... }}]}',
        "```",
        "4. If no tool is needed, respond with standard markdown text."
    ])
    return "\n".join(lines)


def parse_tool_calls_from_text(text):
    """Detect if the response contains a JSON tool_calls block."""
    trimmed = text.strip()
    if "```json" in trimmed:
        parts = trimmed.split("```json")
        for p in parts[1:]:
            cand = p.split("```")[0].strip()
            try:
                data = json.loads(cand)
                if isinstance(data, dict) and "tool_calls" in data:
                    return data["tool_calls"]
            except Exception:
                pass
    elif trimmed.startswith("{") and trimmed.endswith("}"):
        try:
            data = json.loads(trimmed)
            if isinstance(data, dict) and "tool_calls" in data:
                return data["tool_calls"]
        except Exception:
            pass
    return None


AUTH_KEY = os.environ.get("MUSE_SIDECAR_KEY", "").strip()


class RequestHandler(BaseHTTPRequestHandler):
    def _check_security(self):
        # 1. Reject cross-origin requests from browsers (anti-SSRF / anti-CSRF)
        origin = self.headers.get("Origin")
        if origin:
            parsed = urllib.parse.urlparse(origin) if "urllib" in globals() else None
            hostname = parsed.hostname if parsed else origin
            if hostname not in ("localhost", "127.0.0.1", "::1"):
                self.send_response(403)
                self.send_header("Content-Type", "application/json")
                self.end_headers()
                self.wfile.write(b'{"error": "Forbidden: cross-origin access rejected"}')
                return False

        # 2. Check auth token (fail-closed: strictly required on all endpoints)
        if not AUTH_KEY:
            self.send_response(500)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(b'{"error": "Server error: MUSE_SIDECAR_KEY not configured"}')
            return False

        auth_hdr = self.headers.get("Authorization", "")
        expected = f"Bearer {AUTH_KEY}"
        if not hmac.compare_digest(auth_hdr, expected):
            self.send_response(401)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(b'{"error": "Unauthorized: invalid or missing bearer token"}')
            return False
        return True

    def do_OPTIONS(self):
        # Reject CORS preflight: native ccat does not use CORS
        self.send_response(403)
        self.end_headers()

    def do_GET(self):
        if not self._check_security():
            return

        if self.path in ("/health", "/v1/health"):
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            resp = {
                "status": "ok",
                "service": "ccat-muse-sidecar",
                "version": "1.0",
                "auth": "bearer",
            }
            self.wfile.write(json.dumps(resp).encode("utf-8"))
            return

        if self.path == "/v1/models":
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            resp = {
                "object": "list",
                "data": [
                    {"id": "muse-chat", "object": "model", "owned_by": "museai"}
                ]
            }
            self.wfile.write(json.dumps(resp).encode("utf-8"))
            return

        self.send_response(404)
        self.end_headers()

    def do_POST(self):
        if not self._check_security():
            return

        if self.path in ("/reload", "/v1/reload"):
            BRIDGE.reset_gateway()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(b'{"status": "ok", "reloaded": true}')
            return

        if self.path != "/v1/chat/completions":
            self.send_response(404)
            self.end_headers()
            return

        # Enforce application/json for POST requests
        content_type = self.headers.get("Content-Type", "")
        if not content_type.startswith("application/json"):
            self.send_response(415)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(b'{"error": "Unsupported Media Type: expected application/json"}')
            return

        content_length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(content_length).decode("utf-8")
        req = json.loads(body)

        model = req.get("model", "muse-chat")
        messages = req.get("messages", [])
        tools = req.get("tools", [])
        stream = req.get("stream", True)

        prompt_parts = []
        if tools:
            prompt_parts.append(format_tools_prompt(tools))

        for m in messages:
            role = m.get("role", "user")
            content = m.get("content", "")
            if role == "system":
                prompt_parts.append(f"[System Instructions]\n{content}")
            elif role == "user":
                prompt_parts.append(f"[User]\n{content}")
            elif role == "assistant":
                prompt_parts.append(f"[Assistant]\n{content}")

        full_prompt = "\n\n".join(prompt_parts)

        cmpl_id = f"chatcmpl-muse-{int(time.time()*1000)}"

        if stream:
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Cache-Control", "no-cache")
            self.send_header("Connection", "keep-alive")
            self.end_headers()

            accumulated = []
            has_tools = bool(tools)

            try:
                for kind, chunk in BRIDGE.execute_chat(full_prompt):
                    if kind == 'delta':
                        accumulated.append(chunk)
                        # If tools are not requested, stream text chunks immediately
                        if not has_tools:
                            chunk_obj = {
                                "id": cmpl_id,
                                "object": "chat.completion.chunk",
                                "created": int(time.time()),
                                "model": model,
                                "choices": [{"index": 0, "delta": {"content": chunk}, "finish_reason": None}]
                            }
                            self.wfile.write(f"data: {json.dumps(chunk_obj)}\n\n".encode("utf-8"))
                            self.wfile.flush()

                    elif kind == 'done':
                        full_text = chunk
                        if has_tools:
                            tool_calls = parse_tool_calls_from_text(full_text)
                            if tool_calls:
                                formatted_tcs = []
                                for idx, tc in enumerate(tool_calls):
                                    name = tc.get("name", "")
                                    args = tc.get("arguments", {})
                                    if isinstance(args, dict):
                                        args_str = json.dumps(args)
                                    else:
                                        args_str = str(args)
                                    formatted_tcs.append({
                                        "index": idx,
                                        "id": f"call_{idx}_{int(time.time())}",
                                        "type": "function",
                                        "function": {
                                            "name": name,
                                            "arguments": args_str
                                        }
                                    })
                                chunk_obj = {
                                    "id": cmpl_id,
                                    "object": "chat.completion.chunk",
                                    "created": int(time.time()),
                                    "model": model,
                                    "choices": [{"index": 0, "delta": {"tool_calls": formatted_tcs}, "finish_reason": "tool_calls"}]
                                }
                                self.wfile.write(f"data: {json.dumps(chunk_obj)}\n\n".encode("utf-8"))
                                self.wfile.flush()
                            else:
                                # Normal text response
                                chunk_obj = {
                                    "id": cmpl_id,
                                    "object": "chat.completion.chunk",
                                    "created": int(time.time()),
                                    "model": model,
                                    "choices": [{"index": 0, "delta": {"content": full_text}, "finish_reason": "stop"}]
                                }
                                self.wfile.write(f"data: {json.dumps(chunk_obj)}\n\n".encode("utf-8"))
                                self.wfile.flush()

                        self.wfile.write(b"data: [DONE]\n\n")
                        self.wfile.flush()

            except Exception as e:
                err_obj = {"error": {"message": str(e), "type": "muse_error"}}
                self.wfile.write(f"data: {json.dumps(err_obj)}\n\n".encode("utf-8"))
                self.wfile.flush()
        else:
            # Non-streaming response
            full_text = ""
            for kind, chunk in BRIDGE.execute_chat(full_prompt):
                if kind == 'done':
                    full_text = chunk
            tool_calls = parse_tool_calls_from_text(full_text) if tools else None
            msg_obj = {"role": "assistant"}
            if tool_calls:
                msg_obj["tool_calls"] = tool_calls
            else:
                msg_obj["content"] = full_text

            resp = {
                "id": cmpl_id,
                "object": "chat.completion",
                "created": int(time.time()),
                "model": model,
                "choices": [{
                    "index": 0,
                    "message": msg_obj,
                    "finish_reason": "tool_calls" if tool_calls else "stop"
                }],
                "usage": {"prompt_tokens": len(full_prompt)//4, "completion_tokens": len(full_text)//4, "total_tokens": (len(full_prompt)+len(full_text))//4}
            }
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(json.dumps(resp).encode("utf-8"))


def main():
    global AUTH_KEY
    parser = argparse.ArgumentParser(description="MuseAI Sidecar Server")
    parser.add_argument("--port", type=int, default=18615, help="Port to listen on (default 18615)")
    parser.add_argument("--host", default="127.0.0.1", help="Host address (default 127.0.0.1)")
    parser.add_argument("--key", default=None, help="Optional bearer token (prefers MUSE_SIDECAR_KEY env)")
    args = parser.parse_args()

    if not AUTH_KEY and args.key:
        AUTH_KEY = args.key.strip()

    if not AUTH_KEY:
        print("Fatal error: MUSE_SIDECAR_KEY environment variable is required. Failing closed.", file=sys.stderr)
        sys.exit(1)

    server = HTTPServer((args.host, args.port), RequestHandler)
    print(f"MuseAI Sidecar listening on http://{args.host}:{args.port}")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\nShutting down MuseAI Sidecar...")
        server.server_close()


if __name__ == "__main__":
    main()
