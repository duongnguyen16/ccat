#!/usr/bin/env python3
"""Cross-platform quota status line for Claude Code and Antigravity CLI (agy).

Usage:
  Claude Code: statusLine.command = ".../statusline/quota-statusline.sh" (~/.claude/settings.json)
  Antigravity: statusLine.command = ".../statusline/quota-statusline.sh -Agy" (~/.gemini/antigravity-cli/settings.json)
"""

import sys
import os
import json
from datetime import datetime, timezone
from pathlib import Path

SEP = " │ "  # │
DOT = " · "  # ·
I_CTX = "◔"     # ◔
I_CLAUDE = "✻"  # ✻
I_AG = "▲"      # ▲
I_GEM = "✦"     # ✦
I_3P = "◇"      # ◇
I_5H = "5h"
I_WK = "7d"
I_STALE = "~"
FULL = "█"      # █
EMPTY = "░"     # ░
EIGHTHS = ['', '▏', '▎', '▍', '▌', '▋', '▊', '▉']


def pct(used: float) -> str:
    p = int(round(used))
    u = min(64, max(0, int(round(max(0.0, used) * 64 / 100))))
    if used > 0 and u == 0:
        u = 1
    bar = (FULL * (u // 8)) + EIGHTHS[u % 8]
    rest = 8 - (u // 8) - (1 if u % 8 != 0 else 0)
    c = 31 if p >= 80 else (33 if p >= 50 else 32)
    return f"\x1b[{c}m{bar}\x1b[0m\x1b[2m{EMPTY * rest}\x1b[0m \x1b[{c}m{p}%\x1b[0m"


def ag_buckets(quota: dict, prefix: str) -> str:
    now = datetime.now(timezone.utc)
    parts = []
    for k in ("five", "weekly"):
        name = f"{prefix}-5h" if k == "five" else f"{prefix}-weekly"
        b = quota.get(name)
        if not b or not isinstance(b, dict):
            continue
        reset_str = b.get("reset_time", "")
        used = 0.0
        try:
            reset_dt = datetime.fromisoformat(reset_str.replace("Z", "+00:00"))
            if reset_dt > now:
                rem = float(b.get("remaining_fraction", 0.0))
                used = (1.0 - rem) * 100.0
        except Exception:
            used = (1.0 - float(b.get("remaining_fraction", 0.0))) * 100.0
        tag = I_5H if k == "five" else I_WK
        parts.append(f"{tag} {pct(used)}")
    return " ".join(parts)


def ag_line(quota: dict, show_3p: bool = True, show_gemini: bool = True) -> str:
    items = []
    if show_gemini:
        g = ag_buckets(quota, "gemini")
        if g:
            items.append(f"{I_GEM} Gemini {g}")
    if show_3p:
        t = ag_buckets(quota, "3p")
        if t:
            items.append(f"{I_3P} Claude/GPT {t}")
    return DOT.join(items)


def ccat_uses_3p() -> bool:
    appdata = os.environ.get("APPDATA")
    if appdata:
        cfg_file = Path(appdata) / "ccat" / "config.json"
    else:
        xdg = os.environ.get("XDG_CONFIG_HOME")
        base = Path(xdg) if xdg else Path.home() / ".config"
        cfg_file = base / "ccat" / "config.json"

    if not cfg_file.is_file():
        return False
    try:
        data = json.loads(cfg_file.read_text(encoding="utf-8"))
        for t in data.get("tiers", []):
            if isinstance(t, dict):
                provider = t.get("provider", "")
                model = str(t.get("model", "")).strip()
                if provider == "Antigravity" and model and not model.startswith("gemini"):
                    return True
    except Exception:
        pass
    return False


def main():
    is_agy = any(arg.lower() in ("-agy", "--agy") for arg in sys.argv[1:])
    home = Path.home()
    cache_file = home / ".claude" / "cache" / "antigravity-quota.json"

    raw_input = sys.stdin.read().strip()
    if not raw_input:
        return

    try:
        data = json.loads(raw_input)
    except Exception:
        return

    if is_agy:
        quota = data.get("quota")
        if quota:
            cache_file.parent.mkdir(parents=True, exist_ok=True)
            payload = {
                "captured_at": datetime.now(timezone.utc).isoformat(),
                "plan": data.get("plan_tier", ""),
                "quota": quota,
            }
            try:
                cache_file.write_text(json.dumps(payload, indent=2), encoding="utf-8")
            except Exception:
                pass
        line = []
        model_display = data.get("model", {}).get("display_name")
        if model_display:
            line.append(model_display)
        ctx = data.get("context_window", {}).get("used_percentage")
        if ctx is not None:
            line.append(f"{I_CTX} {pct(float(ctx))}")
        if quota:
            al = ag_line(quota)
            if al:
                line.append(al)
        print(SEP.join(line))
        return

    # Claude Code mode
    line = []
    ponytail_file = home / ".claude" / ".ponytail-active"
    if ponytail_file.is_file():
        try:
            mode = ponytail_file.read_text(encoding="utf-8").strip()
            suffix = f":{mode.upper()}" if mode and mode != "full" else ""
            line.append(f"\x1b[38;5;108m[PONYTAIL{suffix}]\x1b[0m")
        except Exception:
            line.append("\x1b[38;5;108m[PONYTAIL]\x1b[0m")

    ctx = data.get("context_window", {}).get("used_percentage")
    if ctx is not None:
        line.append(f"{I_CTX} {pct(float(ctx))}")

    rl = data.get("rate_limits")
    if isinstance(rl, dict):
        c = []
        five_h = rl.get("five_hour")
        if isinstance(five_h, dict) and five_h.get("used_percentage") is not None:
            c.append(f"{I_5H} {pct(float(five_h['used_percentage']))}")
        seven_d = rl.get("seven_day")
        if isinstance(seven_d, dict) and seven_d.get("used_percentage") is not None:
            c.append(f"{I_WK} {pct(float(seven_d['used_percentage']))}")
        if c:
            line.append(f"{I_CLAUDE} Claude {' '.join(c)}")

    if cache_file.is_file():
        try:
            ag_data = json.loads(cache_file.read_text(encoding="utf-8"))
            captured_at_str = ag_data.get("captured_at", "")
            captured_dt = datetime.fromisoformat(captured_at_str.replace("Z", "+00:00"))
            age_secs = (datetime.now(timezone.utc) - captured_dt).total_seconds()
            stale_hours = int(age_secs / 3600)
            stale = f" \x1b[2m{I_STALE}{stale_hours}h\x1b[0m" if stale_hours >= 1 else ""
            text = ag_line(ag_data.get("quota", {}), show_3p=ccat_uses_3p(), show_gemini=False)
            if text:
                line.append(f"{I_AG} {text}{stale}")
        except Exception:
            pass

    print(SEP.join(line))


if __name__ == "__main__":
    main()
