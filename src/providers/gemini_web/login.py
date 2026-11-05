#!/usr/bin/env python3
"""
login.py
Gemini Web Login & Cookie Sync for ccat.
Stores named accounts and DPAPI-encrypted cookies in %APPDATA%/ccat/gemini_web.
"""

import argparse
import ctypes
import hashlib
import json
import os
import re
import sys
import time
import uuid
from ctypes import wintypes
from pathlib import Path

if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8")
if hasattr(sys.stderr, "reconfigure"):
    sys.stderr.reconfigure(encoding="utf-8")

BASE_DIR = Path(__file__).resolve().parent
ACCOUNTS_DIR = BASE_DIR / "accounts"
PROFILES_DIR = BASE_DIR / "profiles"
REGISTRY = ACCOUNTS_DIR / "accounts.json"

REQUIRED_COOKIES = {"__Secure-1PSID", "__Secure-1PSIDTS"}
USEFUL_COOKIES = {
    "__Secure-1PSID", "__Secure-1PSIDTS", "__Secure-1PSIDCC",
    "__Secure-3PSID", "__Secure-3PSIDTS", "__Secure-3PSIDCC",
    "SID", "HSID", "SSID", "APISID", "SAPISID",
    "__Secure-1PAPISID", "__Secure-3PAPISID",
    "NID", "1P_JAR", "AEC", "SOCS",
}
NAME_RE = re.compile(r"^[\w .-]{1,48}$", re.UNICODE)


class DATA_BLOB(ctypes.Structure):
    _fields_ = [("cbData", wintypes.DWORD), ("pbData", ctypes.POINTER(ctypes.c_byte))]


def _dpapi(data: bytes, protect: bool) -> bytes:
    if os.name != "nt":
        raise RuntimeError("Named account storage requires Windows DPAPI.")
    crypt32 = ctypes.WinDLL("crypt32", use_last_error=True)
    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    source_buf = ctypes.create_string_buffer(data)
    source = DATA_BLOB(len(data), ctypes.cast(source_buf, ctypes.POINTER(ctypes.c_byte)))
    target = DATA_BLOB()
    fn = crypt32.CryptProtectData if protect else crypt32.CryptUnprotectData
    fn.argtypes = [
        ctypes.POINTER(DATA_BLOB), wintypes.LPCWSTR, ctypes.c_void_p,
        ctypes.c_void_p, ctypes.c_void_p, wintypes.DWORD, ctypes.POINTER(DATA_BLOB)
    ] if protect else [
        ctypes.POINTER(DATA_BLOB), ctypes.POINTER(wintypes.LPWSTR),
        ctypes.c_void_p, ctypes.c_void_p, ctypes.c_void_p, wintypes.DWORD, ctypes.POINTER(DATA_BLOB)
    ]
    fn.restype = wintypes.BOOL
    ok = fn(ctypes.byref(source), "Argon Gemini account", None, None, None, 0x1, ctypes.byref(target)) if protect else fn(
        ctypes.byref(source), None, None, None, None, 0x1, ctypes.byref(target)
    )
    if not ok:
        raise ctypes.WinError(ctypes.get_last_error())
    try:
        return ctypes.string_at(target.pbData, target.cbData)
    finally:
        kernel32.LocalFree(target.pbData)


def list_accounts() -> list[dict]:
    if not REGISTRY.exists():
        return []
    try:
        data = json.loads(REGISTRY.read_text(encoding="utf-8"))
        return data if isinstance(data, list) else []
    except Exception:
        return []


def save_account(name: str, cookie_header: str, profile_dir: Path) -> dict:
    name = name.strip()
    if not NAME_RE.fullmatch(name):
        raise ValueError("Account name must be 1-48 alphanumeric characters, spaces, dots, underscores or dashes.")
    ACCOUNTS_DIR.mkdir(parents=True, exist_ok=True)
    records = list_accounts()
    existing = next((r for r in records if r.get("name", "").casefold() == name.casefold()), None)
    reserved_ports = {4981, 4988}
    reserved_ports.update(int(r.get("port", 0)) for r in records)
    port = existing.get("port") if existing else next(p for p in range(4982, 65536) if p not in reserved_ports)
    record = existing or {
        "id": uuid.uuid4().hex,
        "name": name,
        "port": port,
        "enabled": True,
        "request_count": 0,
        "error_count": 0,
    }
    dpapi_path = ACCOUNTS_DIR / f"{record['id']}.dpapi"
    dpapi_path.write_bytes(_dpapi(cookie_header.encode("utf-8"), True))
    record["profile"] = str(profile_dir.name)
    if existing is None:
        records.append(record)
    temp = REGISTRY.with_suffix(".tmp")
    temp.write_text(json.dumps(records, indent=2, ensure_ascii=False), encoding="utf-8")
    temp.replace(REGISTRY)
    return record


def account_profile_dir(name: str) -> Path:
    key = hashlib.sha256(name.strip().casefold().encode("utf-8")).hexdigest()[:20]
    return PROFILES_DIR / key


def extract_cookies_from_context(context) -> dict[str, str]:
    raw = context.cookies(["https://gemini.google.com", "https://accounts.google.com", "https://google.com"])
    cookies = {}
    for c in raw:
        if ".google.com" in c.get("domain", "") and c["name"] not in cookies:
            cookies[c["name"]] = c["value"]
    return cookies


def interactive_login(account_name: str, timeout_sec: int = 300) -> bool:
    from playwright.sync_api import sync_playwright

    print("=" * 60)
    print(f"  🌐 Gemini Web 1-Click Login: {account_name}")
    print("=" * 60)
    print("Opening Google Chrome login window...")
    print("Please log in to your Google / Gemini account.")
    print("After logging in, cookies will be captured and encrypted via Windows DPAPI.\n")

    profile_dir = account_profile_dir(account_name)
    profile_dir.mkdir(parents=True, exist_ok=True)

    with sync_playwright() as p:
        ctx = p.chromium.launch_persistent_context(
            user_data_dir=str(profile_dir),
            channel="chrome",
            headless=False,
            args=[
                "--disable-blink-features=AutomationControlled",
                "--no-first-run",
                "--no-default-browser-check",
            ],
            ignore_default_args=["--enable-automation"],
            no_viewport=True,
        )
        page = ctx.new_page()
        page.goto("https://gemini.google.com/app")

        start = time.time()
        success = False
        while time.time() - start < timeout_sec:
            cookies = extract_cookies_from_context(ctx)
            if all(k in cookies for k in REQUIRED_COOKIES):
                header = "; ".join(f"{k}={v}" for k, v in cookies.items() if k in USEFUL_COOKIES)
                record = save_account(account_name, header, profile_dir)
                print("\n" + "=" * 60)
                print(f"  [OK] Login successful for '{account_name}' (Port {record['port']})!")
                print(f"  Encrypted credentials saved in %APPDATA%/ccat/gemini_web/accounts.")
                print("=" * 60)
                success = True
                time.sleep(1.5)
                break
            time.sleep(1)

        ctx.close()
        if not success:
            print(f"\n[FAIL] Login timed out after {timeout_sec}s.")
        return success


def refresh_headless(account_name: str) -> bool:
    from playwright.sync_api import sync_playwright

    profile_dir = account_profile_dir(account_name)
    if not profile_dir.exists():
        return False
    try:
        with sync_playwright() as p:
            ctx = p.chromium.launch_persistent_context(
                user_data_dir=str(profile_dir),
                channel="chrome",
                headless=True,
                args=[
                    "--disable-blink-features=AutomationControlled",
                    "--no-first-run",
                    "--disable-gpu",
                ],
                ignore_default_args=["--enable-automation"],
                no_viewport=True,
            )
            page = ctx.new_page()
            page.goto("https://gemini.google.com/app", timeout=20000, wait_until="domcontentloaded")
            time.sleep(2)
            cookies = extract_cookies_from_context(ctx)
            ctx.close()
            if all(k in cookies for k in REQUIRED_COOKIES):
                header = "; ".join(f"{k}={v}" for k, v in cookies.items() if k in USEFUL_COOKIES)
                save_account(account_name, header, profile_dir)
                return True
    except Exception as e:
        print(f"Cookie refresh failed: {e}", file=sys.stderr)
    return False


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description="ccat Gemini Web account login")
    parser.add_argument("--account", required=True, help="Account name")
    parser.add_argument("--refresh", action="store_true", help="Headless cookie refresh")
    args = parser.parse_args()

    if args.refresh:
        ok = refresh_headless(args.account)
    else:
        ok = interactive_login(args.account)
    sys.exit(0 if ok else 1)
