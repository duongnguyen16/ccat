# One status line script for Claude Code and the Antigravity CLI (agy).
#   agy:          statusLine.command = "... quota-statusline.ps1 -Agy"  (~/.gemini/antigravity-cli/settings.json)
#                 -> saves the quota agy passes on stdin to a cache file, prints agy's status line
#   Claude Code:  statusLine.command = "... quota-statusline.ps1"        (~/.claude/settings.json)
#                 -> prints context + Claude plan limits + the cached Antigravity quota
# ponytail: the Antigravity quota only refreshes while agy is open; quota spent elsewhere shows after the next agy start.
# Keep this file ASCII: Windows PowerShell 5.1 misreads UTF-8 without a BOM, so glyphs are built from code points.
param([switch]$Agy)
[Console]::InputEncoding = [Console]::OutputEncoding = [Text.Encoding]::UTF8
$sep = " $([char]0x2502) "; $dot = " $([char]0xB7) "
function Ic([int]$cp) { [char]::ConvertFromUtf32($cp) }
$iCtx = Ic 0x25D4; $iClaude = Ic 0x273B; $iAg = Ic 0x25B2; $iGem = Ic 0x2726; $i3p = Ic 0x25C7
$i5h = '5h'; $iWk = '7d'; $iStale = '~'
$full = Ic 0x2588; $empty = Ic 0x2591
$eighths = @('') + (0x258F, 0x258E, 0x258D, 0x258C, 0x258B, 0x258A, 0x2589 | ForEach-Object { Ic $_ })  # 1/8 .. 7/8 of a cell
$cache = Join-Path $env:USERPROFILE '.claude\cache\antigravity-quota.json'
$in = [Console]::In.ReadToEnd().TrimStart([char]0xFEFF) | ConvertFrom-Json

# 8-cell bar, 1/8-cell resolution: full + partial block (colored) = used, light shade (dim) = left
function Pct([double]$used) {
    $p = [int][Math]::Round($used)
    $u = [Math]::Min(64, [Math]::Max(0, [int][Math]::Round([Math]::Max(0, $used) * 64 / 100)))
    if ($used -gt 0 -and $u -eq 0) { $u = 1 }  # anything used shows at least 1/8 cell
    $bar = ($full * [Math]::Floor($u / 8)) + $eighths[$u % 8]
    $rest = 8 - [Math]::Floor($u / 8) - [int]($u % 8 -ne 0)
    $c = if ($p -ge 80) { 31 } elseif ($p -ge 50) { 33 } else { 32 }
    $e = [char]27
    "$e[${c}m$bar$e[0m$e[2m$($empty * $rest)$e[0m $e[${c}m$p%$e[0m"
}

# "gemini-5h" -> "5h <bar>"; a window past its reset_time counts as 0%
function AgBuckets($quota, [string]$prefix) {
    $now = [DateTimeOffset]::UtcNow
    $parts = foreach ($k in 'five', 'weekly') {
        $name = if ($k -eq 'five') { "$prefix-5h" } else { "$prefix-weekly" }
        $b = $quota.$name
        if (-not $b) { continue }
        $used = if ([DateTimeOffset]::Parse($b.reset_time) -le $now) { 0 } else { (1 - $b.remaining_fraction) * 100 }
        "$(if ($k -eq 'five') { $i5h } else { $iWk }) $(Pct $used)"
    }
    $parts -join ' '
}

function AgLine($quota, [bool]$show3p = $true, [bool]$showGemini = $true) {
    $g = if ($showGemini) { AgBuckets $quota 'gemini' }
    $t = if ($show3p) { AgBuckets $quota '3p' }
    (@(if ($g) { "$iGem Gemini $g" }; if ($t) { "$i3p Claude/GPT $t" })) -join $dot
}

# The 3P bucket (Claude/GPT on Antigravity) only matters when a ccat tier runs a non-Gemini model on Antigravity
function CcatUses3p {
    $f = Join-Path $env:APPDATA 'ccat\config.json'
    if (-not (Test-Path $f)) { return $false }
    foreach ($t in (Get-Content $f -Raw | ConvertFrom-Json).tiers) {
        if ($t.provider -eq 'Antigravity' -and "$($t.model)".Trim() -and "$($t.model)" -notmatch '^gemini') { return $true }
    }
    $false
}

if ($Agy) {
    if ($in.quota) {
        New-Item -ItemType Directory -Force -Path (Split-Path $cache) | Out-Null
        @{ captured_at = [DateTimeOffset]::UtcNow.ToString('o'); plan = $in.plan_tier; quota = $in.quota } |
            ConvertTo-Json -Depth 5 | Set-Content $cache -Encoding utf8
    }
    $line = @($in.model.display_name, "$iCtx $(Pct $in.context_window.used_percentage)")
    if ($in.quota) { $line += AgLine $in.quota }
    $line -join $sep
    exit
}

$line = @()
# The ponytail plugin's badge: its own statusLine would be replaced by this one
$pt = Join-Path $env:USERPROFILE '.claude\.ponytail-active'
if (Test-Path $pt) {
    $mode = "$(Get-Content $pt -TotalCount 1)".Trim()
    $line = @("$([char]27)[38;5;108m[PONYTAIL$(if ($mode -and $mode -ne 'full') { ':' + $mode.ToUpperInvariant() })]$([char]27)[0m") + $line
}
if ($null -ne $in.context_window.used_percentage) { $line += "$iCtx $(Pct $in.context_window.used_percentage)" }
$rl = $in.rate_limits
if ($rl) {
    $c = @()
    if ($rl.five_hour) { $c += "$i5h $(Pct $rl.five_hour.used_percentage)" }
    if ($rl.seven_day) { $c += "$iWk $(Pct $rl.seven_day.used_percentage)" }
    if ($c) { $line += "$iClaude Claude $($c -join ' ')" }
}
if (Test-Path $cache) {
    $ag = Get-Content $cache -Raw | ConvertFrom-Json
    $age = [DateTimeOffset]::UtcNow - [DateTimeOffset]::Parse($ag.captured_at)
    $stale = if ($age.TotalHours -ge 1) { " $([char]27)[2m$iStale$([int]$age.TotalHours)h$([char]27)[0m" } else { '' }
    # The Gemini bucket stays in agy's own line only; here it is just the Claude/GPT one, when a tier uses it
    $text = AgLine $ag.quota (CcatUses3p) $false
    if ($text) { $line += "$iAg $text$stale" }
}
$line -join $sep
