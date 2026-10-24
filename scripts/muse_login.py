#!/usr/bin/env python3
"""
MuseAI one-click browser login helper for ccat.
Launches dedicated Chrome on a loopback CDP port with a clean per-login profile,
assigns process to a Windows Job Object (ensuring zero orphaned browsers),
polls for hatch_sess cookie, validates session against muse.ai via curl_cffi,
atomically updates cookies.txt with owner-only permissions, and outputs non-secret account metadata JSON.
"""

import argparse
import base64
import json
import os
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request

try:
    from curl_cffi import requests as rq
    from muse_cli.gateway import _hatch_headers, fetch_access_token, fetch_session_info
except ImportError:
    # Stable app-owned venv fallback if invoked via system python
    appdata = os.environ.get("APPDATA") or os.path.expanduser("~/.config")
    site_pkgs = os.path.join(appdata, "ccat", "muse-venv", "Lib", "site-packages")
    if os.path.exists(site_pkgs) and site_pkgs not in sys.path:
        sys.path.insert(0, site_pkgs)
    from curl_cffi import requests as rq
    from muse_cli.gateway import _hatch_headers, fetch_access_token, fetch_session_info

ALLOWED_DOMAINS = ("muse.ai", "metaaivm.com")


def is_allowed_domain(domain):
    """Enforce exact host or approved .suffix boundary; reject substring lookalikes."""
    if not domain or not isinstance(domain, str):
        return False
    d = domain.lower().lstrip(".")
    for allowed in ALLOWED_DOMAINS:
        if d == allowed or d.endswith("." + allowed):
            return True
    return False


def secure_file(path):
    """Restrict file permissions to current user only (0600 on Unix, explicit user ACL on Windows)."""
    if sys.platform != "win32":
        os.chmod(path, 0o600)
        return

    user = os.environ.get("USERNAME")
    if not user:
        profile = os.environ.get("USERPROFILE", "")
        user = os.path.basename(profile) if profile else None
    if not user:
        raise RuntimeError("Could not determine current Windows user for file ACL protection")

    r = subprocess.run(
        ["icacls", path, "/inheritance:r", "/grant:r", f"{user}:(F)", "*S-1-5-18:(F)"],
        capture_output=True,
    )
    if r.returncode != 0:
        raise RuntimeError(f"icacls failed on file {path}: {r.stderr.decode(errors='replace')}")


def secure_directory(path):
    """Restrict directory permissions to current user only (0700 on Unix, explicit user ACL on Windows)."""
    if sys.platform != "win32":
        os.chmod(path, 0o700)
        return

    user = os.environ.get("USERNAME")
    if not user:
        profile = os.environ.get("USERPROFILE", "")
        user = os.path.basename(profile) if profile else None
    if not user:
        raise RuntimeError("Could not determine current Windows user for directory ACL protection")

    r = subprocess.run(
        ["icacls", path, "/inheritance:r", "/grant:r", f"{user}:(OI)(CI)(F)", "*S-1-5-18:(OI)(CI)(F)"],
        capture_output=True,
    )
    if r.returncode != 0:
        raise RuntimeError(f"icacls failed on directory {path}: {r.stderr.decode(errors='replace')}")


def assign_to_job_object(pid):
    """Bind child process to a Windows Job Object configured to terminate on parent exit."""
    if sys.platform != "win32":
        return None
    try:
        import ctypes
        from ctypes import wintypes

        kernel32 = ctypes.windll.kernel32
        job = kernel32.CreateJobObjectW(None, None)
        if not job:
            return None

        class JOBOBJECT_BASIC_LIMIT_INFORMATION(ctypes.Structure):
            _fields_ = [
                ("PerProcessUserTimeLimit", wintypes.LARGE_INTEGER),
                ("PerJobUserTimeLimit", wintypes.LARGE_INTEGER),
                ("LimitFlags", wintypes.DWORD),
                ("MinimumWorkingSetSize", ctypes.c_size_t),
                ("MaximumWorkingSetSize", ctypes.c_size_t),
                ("ActiveProcessLimit", wintypes.DWORD),
                ("Affinity", ctypes.c_size_t),
                ("PriorityClass", wintypes.DWORD),
                ("SchedulingClass", wintypes.DWORD),
            ]

        class IO_COUNTERS(ctypes.Structure):
            _fields_ = [
                ("ReadOperationCount", wintypes.ULARGE_INTEGER),
                ("WriteOperationCount", wintypes.ULARGE_INTEGER),
                ("OtherOperationCount", wintypes.ULARGE_INTEGER),
                ("ReadTransferCount", wintypes.ULARGE_INTEGER),
                ("WriteTransferCount", wintypes.ULARGE_INTEGER),
                ("OtherTransferCount", wintypes.ULARGE_INTEGER),
            ]

        class JOBOBJECT_EXTENDED_LIMIT_INFORMATION(ctypes.Structure):
            _fields_ = [
                ("BasicLimitInformation", JOBOBJECT_BASIC_LIMIT_INFORMATION),
                ("IoCounters", IO_COUNTERS),
                ("ProcessMemoryLimit", ctypes.c_size_t),
                ("JobMemoryLimit", ctypes.c_size_t),
                ("PeakProcessMemoryLimit", ctypes.c_size_t),
                ("PeakJobMemoryLimit", ctypes.c_size_t),
            ]

        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE = 0x00002000
        JobObjectExtendedLimitInformation = 9

        info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION()
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE

        kernel32.SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            ctypes.byref(info),
            ctypes.sizeof(info),
        )

        h_proc = kernel32.OpenProcess(0x1F0FFF, False, pid)
        if h_proc:
            kernel32.AssignProcessToJobObject(job, h_proc)
            kernel32.CloseHandle(h_proc)
        return job
    except Exception:
        return None


def find_chrome(override_path=None):
    if override_path and os.path.isfile(override_path):
        return override_path
    env_path = os.environ.get("CHROME_PATH")
    if env_path and os.path.isfile(env_path):
        return env_path

    if sys.platform == "win32":
        try:
            import winreg
            for root in (winreg.HKEY_LOCAL_MACHINE, winreg.HKEY_CURRENT_USER):
                try:
                    with winreg.OpenKey(root, r"SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\chrome.exe") as k:
                        val = winreg.QueryValue(k, None)
                        if val and os.path.isfile(val):
                            return val
                except OSError:
                    pass
        except ImportError:
            pass

        candidates = [
            r"C:\Program Files\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
            os.path.expandvars(r"%LOCALAPPDATA%\Google\Chrome\Application\chrome.exe"),
        ]
        for c in candidates:
            if os.path.isfile(c):
                return c
    elif sys.platform == "darwin":
        mac_path = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
        if os.path.isfile(mac_path):
            return mac_path
    else:
        for b in ("google-chrome", "google-chrome-stable", "chromium", "chromium-browser"):
            p = shutil.which(b)
            if p:
                return p
    return None


def get_free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


class CDPClient:
    """Minimal stdlib RFC 6455 WebSocket client for Chrome DevTools Protocol."""

    def __init__(self, ws_url):
        assert ws_url.startswith("ws://"), f"Invalid ws_url: {ws_url}"
        rest = ws_url[5:]
        host_port, path = rest.split("/", 1)
        host, port = host_port.split(":")
        self.host = host
        self.port = int(port)
        self.path = "/" + path
        self.sock = None
        self._next_id = 1
        self._connect()

    def _connect(self):
        s = socket.create_connection((self.host, self.port), timeout=5)
        key = base64.b64encode(os.urandom(16)).decode()
        handshake = (
            f"GET {self.path} HTTP/1.1\r\n"
            f"Host: {self.host}:{self.port}\r\n"
            f"Upgrade: websocket\r\n"
            f"Connection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\n"
            f"Sec-WebSocket-Version: 13\r\n\r\n"
        )
        s.sendall(handshake.encode())
        resp = b""
        while b"\r\n\r\n" not in resp:
            chunk = s.recv(4096)
            if not chunk:
                raise RuntimeError("Connection closed during WS handshake")
            resp += chunk
        if b" 101 " not in resp:
            raise RuntimeError(f"WS handshake failed: {resp!r}")
        self.sock = s

    def send_cmd(self, method, params=None):
        req_id = self._next_id
        self._next_id += 1
        payload = json.dumps({"id": req_id, "method": method, "params": params or {}}).encode("utf-8")
        mask = os.urandom(4)
        length = len(payload)
        if length <= 125:
            header = bytes([0x81, 0x80 | length]) + mask
        elif length <= 65535:
            header = bytes([0x81, 0x80 | 126]) + struct.pack("!H", length) + mask
        else:
            header = bytes([0x81, 0x80 | 127]) + struct.pack("!Q", length) + mask

        masked_payload = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
        self.sock.sendall(header + masked_payload)

        def recv_exact(n):
            buf = b""
            while len(buf) < n:
                c = self.sock.recv(n - len(buf))
                if not c:
                    raise RuntimeError("EOF while reading WS frame")
                buf += c
            return buf

        h = recv_exact(2)
        b1, b2 = h[0], h[1]
        masked = bool(b2 & 0x80)
        plen = b2 & 0x7F
        if plen == 126:
            plen = struct.unpack("!H", recv_exact(2))[0]
        elif plen == 127:
            plen = struct.unpack("!Q", recv_exact(8))[0]

        mask_key = recv_exact(4) if masked else None
        data = recv_exact(plen)
        if masked:
            data = bytes(b ^ mask_key[i % 4] for i, b in enumerate(data))
        return json.loads(data.decode("utf-8"))

    def close(self):
        if self.sock:
            try:
                self.sock.close()
            except Exception:
                pass
            self.sock = None


def watch_stdin_for_cancel(cancel_event):
    """Listen for cancel command or pipe close from parent process."""
    try:
        while not cancel_event.is_set():
            line = sys.stdin.readline()
            if not line or line.strip() == "cancel":
                cancel_event.set()
                break
    except Exception:
        cancel_event.set()


def format_netscape_cookies(cookies):
    lines = ["# Netscape HTTP Cookie File"]
    for c in cookies:
        dom = c.get("domain", "")
        flag = "TRUE" if dom.startswith(".") else "FALSE"
        path = c.get("path", "/")
        sec = "TRUE" if c.get("secure") else "FALSE"
        exp = str(int(c.get("expires", 0) or 0))
        name = c.get("name", "")
        val = c.get("value", "")
        lines.append(f"{dom}\t{flag}\t{path}\t{sec}\t{exp}\t{name}\t{val}")
    return "\n".join(lines) + "\n"


def run_login(timeout=300, chrome_path=None, profile_dir=None):
    chrome_exe = find_chrome(chrome_path)
    if not chrome_exe:
        return {"status": "error", "message": "Google Chrome executable not found."}

    # Use fresh per-login profile to avoid capturing stale session before account switch
    owned_temp_profile = False
    if not profile_dir:
        profile_dir = tempfile.mkdtemp(prefix="ccat_muse_chrome_")
        owned_temp_profile = True
    else:
        os.makedirs(profile_dir, exist_ok=True)
    secure_directory(profile_dir)

    config_dir = os.path.expanduser("~/.config/muse-cli")
    os.makedirs(config_dir, exist_ok=True)
    secure_directory(config_dir)

    cookies_path = os.path.join(config_dir, "cookies.txt")
    config_path = os.path.join(config_dir, "config.json")

    port = get_free_port()
    cmd = [
        chrome_exe,
        f"--remote-debugging-port={port}",
        "--remote-debugging-address=127.0.0.1",
        f"--user-data-dir={profile_dir}",
        "--no-first-run",
        "--no-default-browser-check",
        "https://muse.ai/",
    ]

    cancel_event = threading.Event()
    t = threading.Thread(target=watch_stdin_for_cancel, args=(cancel_event,), daemon=True)
    t.start()

    proc = subprocess.Popen(cmd)
    _job = assign_to_job_object(proc.pid)
    cdp = None
    try:
        # Wait for DevTools endpoint
        ws_url = None
        start_wait = time.time()
        while time.time() - start_wait < 15:
            if cancel_event.is_set():
                return {"status": "cancelled", "message": "Sign-in cancelled."}
            if proc.poll() is not None:
                return {"status": "cancelled", "message": "Chrome window was closed before sign-in."}
            try:
                with urllib.request.urlopen(f"http://127.0.0.1:{port}/json/version", timeout=1) as resp:
                    data = json.loads(resp.read().decode())
                    ws_url = data.get("webSocketDebuggerUrl")
                    if ws_url:
                        break
            except Exception:
                time.sleep(0.2)

        if not ws_url:
            return {"status": "error", "message": "Could not connect to Chrome DevTools endpoint."}

        cdp = CDPClient(ws_url)
        deadline = time.time() + timeout

        while time.time() < deadline:
            if cancel_event.is_set():
                return {"status": "cancelled", "message": "Sign-in cancelled."}
            if proc.poll() is not None:
                return {"status": "cancelled", "message": "Chrome window was closed before sign-in."}

            try:
                # Prefer Network.getCookies with fixed Muse endpoints
                res = cdp.send_cmd("Network.getCookies", {"urls": ["https://muse.ai", "https://hatch.metaaivm.com"]})
                cookies = res.get("result", {}).get("cookies", [])
                if not cookies:
                    res = cdp.send_cmd("Storage.getCookies")
                    cookies = res.get("result", {}).get("cookies", [])
            except Exception:
                time.sleep(1.0)
                continue

            # Strict domain boundary filter before inspecting hatch_sess
            filtered_cookies = [
                c for c in cookies
                if is_allowed_domain(c.get("domain"))
            ]

            # Require hatch_sess to strictly belong to muse.ai
            hatch_sess = next(
                (c for c in filtered_cookies
                 if c.get("name") == "hatch_sess"
                 and c.get("domain", "").lower().lstrip(".").endswith("muse.ai")),
                None,
            )

            if hatch_sess and hatch_sess.get("value"):
                # Scope cookies sent to https://muse.ai strictly to muse.ai origin
                muse_cookies = [
                    c for c in filtered_cookies
                    if c.get("domain", "").lower().lstrip(".").endswith("muse.ai")
                ]
                cookie_header = "; ".join(f"{c['name']}={c['value']}" for c in muse_cookies)

                # Validate session against muse.ai before committing
                try:
                    tok = fetch_access_token(cookie_header)
                    auth_res = rq.post(
                        "https://muse.ai/api/auth/check",
                        headers=_hatch_headers(cookie_header),
                        data=b"",
                        impersonate="chrome",
                        timeout=15,
                    )
                    viewer_id = ""
                    if auth_res.status_code == 200:
                        viewer_id = str(auth_res.json().get("viewer_id", ""))

                    sess_info = fetch_session_info(cookie_header)
                    vm_id = sess_info.get("vm_id", "")

                    # Atomic save of cookies.txt with owner-only permissions
                    netscape_text = format_netscape_cookies(filtered_cookies)
                    fd, tmp_file = tempfile.mkstemp(dir=config_dir, prefix="cookies_", suffix=".tmp")
                    with os.fdopen(fd, "w", encoding="utf-8") as f:
                        f.write(netscape_text)
                    secure_file(tmp_file)

                    if os.path.exists(cookies_path):
                        bak_file = cookies_path + ".bak"
                        try:
                            if os.path.exists(bak_file):
                                os.remove(bak_file)
                            os.replace(cookies_path, bak_file)
                            secure_file(bak_file)
                        except Exception:
                            pass
                    os.replace(tmp_file, cookies_path)
                    secure_file(cookies_path)

                    # Update config.json securely
                    cfg = {}
                    if os.path.exists(config_path):
                        try:
                            with open(config_path, "r", encoding="utf-8") as f:
                                cfg = json.load(f)
                        except Exception:
                            pass
                    if vm_id:
                        cfg["vm_id"] = vm_id
                    if viewer_id:
                        cfg["viewer_id"] = viewer_id
                    cfg.setdefault("cookies_file", cookies_path)

                    fd_cfg, tmp_cfg = tempfile.mkstemp(dir=config_dir, prefix="config_", suffix=".tmp")
                    with os.fdopen(fd_cfg, "w", encoding="utf-8") as f:
                        json.dump(cfg, f, indent=2)
                    secure_file(tmp_cfg)
                    os.replace(tmp_cfg, config_path)
                    secure_file(config_path)

                    tier = "Free plan"
                    usage_path = os.path.join(config_dir, "usage.json")
                    if os.path.exists(usage_path):
                        try:
                            with open(usage_path, "r", encoding="utf-8") as f:
                                udata = json.load(f)
                                tier = udata.get("tier", tier)
                        except Exception:
                            pass

                    # Close dedicated Chrome on successful sign-in
                    try:
                        cdp.send_cmd("Browser.close")
                    except Exception:
                        pass

                    return {
                        "status": "ok",
                        "viewer_id": viewer_id,
                        "vm_id": vm_id,
                        "tier": tier,
                    }
                except Exception:
                    time.sleep(1.0)
                    continue

            time.sleep(1.0)

        return {"status": "error", "message": "Sign-in timed out without completing login."}

    finally:
        if cdp:
            cdp.close()
        if proc.poll() is None:
            try:
                proc.terminate()
                proc.wait(timeout=2.0)
            except Exception:
                try:
                    proc.kill()
                except Exception:
                    pass
        if owned_temp_profile and os.path.isdir(profile_dir):
            for _ in range(5):
                try:
                    shutil.rmtree(profile_dir, ignore_errors=True)
                    break
                except Exception:
                    time.sleep(0.3)


def run_tests():
    chrome = find_chrome()
    assert chrome and os.path.exists(chrome), f"Chrome not found: {chrome}"
    port = get_free_port()
    assert port > 0, f"Invalid port: {port}"

    # Security test: domain boundary checks
    assert is_allowed_domain("muse.ai")
    assert is_allowed_domain(".muse.ai")
    assert is_allowed_domain("sub.muse.ai")
    assert is_allowed_domain("hatch.metaaivm.com")
    assert is_allowed_domain(".metaaivm.com")
    assert not is_allowed_domain("evilmuse.ai")
    assert not is_allowed_domain("muse.ai.attacker.com")
    assert not is_allowed_domain("attacker-metaaivm.com")
    assert not is_allowed_domain("")
    assert not is_allowed_domain(None)

    # Netscape cookie format check
    cookies = [
        {"domain": ".muse.ai", "path": "/", "secure": True, "expires": 1800000000, "name": "hatch_sess", "value": "test_val"}
    ]
    netscape = format_netscape_cookies(cookies)
    assert "hatch_sess\ttest_val" in netscape
    assert netscape.startswith("# Netscape HTTP Cookie File")

    # Tempfile and secure file check
    config_dir = os.path.expanduser("~/.config/muse-cli")
    os.makedirs(config_dir, exist_ok=True)
    secure_directory(config_dir)
    fd, tmp_test = tempfile.mkstemp(dir=config_dir, prefix="test_sec_", suffix=".tmp")
    with os.fdopen(fd, "w") as f:
        f.write("test")
    secure_file(tmp_test)
    assert os.path.exists(tmp_test)
    os.remove(tmp_test)

    # Test fresh profile creation and secure_directory
    t_prof = tempfile.mkdtemp(prefix="test_prof_")
    assert os.path.isdir(t_prof)
    secure_directory(t_prof)
    shutil.rmtree(t_prof)
    assert not os.path.exists(t_prof)

    print("muse_login self-tests passed.")


def main():
    parser = argparse.ArgumentParser(description="MuseAI browser login helper")
    parser.add_argument("--test", action="store_true", help="Run self-tests")
    parser.add_argument("--timeout", type=int, default=300, help="Timeout in seconds")
    parser.add_argument("--chrome-path", default=None, help="Path to Chrome executable")
    parser.add_argument("--profile-dir", default=None, help="Dedicated user-data-dir")
    args = parser.parse_args()

    if args.test:
        run_tests()
        sys.exit(0)

    result = run_login(timeout=args.timeout, chrome_path=args.chrome_path, profile_dir=args.profile_dir)
    print(json.dumps(result, ensure_ascii=False))
    if result.get("status") == "ok":
        sys.exit(0)
    elif result.get("status") == "cancelled":
        sys.exit(2)
    else:
        sys.exit(1)


if __name__ == "__main__":
    main()
