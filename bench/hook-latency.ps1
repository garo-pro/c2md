# Measures the cost that actually matters: one full hook invocation, process spawn included, as Claude Code pays it once per turn.
#
# Usage: powershell -File bench/hook-latency.ps1 <fixture.jsonl> [runs]

param(
    [Parameter(Mandatory = $true)][string]$Fixture,
    [int]$Runs = 30
)

$root = Split-Path -Parent $PSScriptRoot
$out = Join-Path $env:TEMP "c2md-bench"
New-Item -ItemType Directory -Force -Path $out | Out-Null

$env:C2MD_AUTO_OPEN = "0"
$env:C2MD_OUTPUT_DIR = $out
$env:C2MD_NODE_OUT = Join-Path $out "node-hook.html"

$payload = @{
    session_id      = "bench"
    transcript_path = (Resolve-Path $Fixture).Path
    cwd             = $root
    hook_event_name = "Stop"
    stop_hook_active = $false
} | ConvertTo-Json -Compress

# Each candidate is a scriptblock so the timing loop stays identical across them.
$candidates = @(
    @{ Name = "c2md (rust)"; Run = { $payload | & (Join-Path $root "target\release\c2md.exe") hook | Out-Null } }
    @{ Name = "node + marked"; Run = { $payload | & node (Join-Path $root "bench\node\hook.mjs") | Out-Null } }
)

Write-Host "hook latency: $Runs runs each, whole process, median reported`n"

foreach ($c in $candidates) {
    if ($c.Name -like "node*" -and -not (Get-Command node -ErrorAction SilentlyContinue)) { continue }

    # One untimed run so the binary and its pages are in the OS file cache.
    Get-ChildItem $out -Filter *.html | Remove-Item -Force -ErrorAction SilentlyContinue
    & $c.Run | Out-Null

    # A candidate that silently bails would otherwise post an impressively fast time for doing nothing.
    $produced = Get-ChildItem $out -Filter *.html -ErrorAction SilentlyContinue
    if (-not $produced) {
        "{0,-16} produced no output, skipped" -f $c.Name | Write-Host
        continue
    }

    $samples = foreach ($i in 1..$Runs) {
        (Measure-Command { & $c.Run }).TotalMilliseconds
    }
    $sorted = $samples | Sort-Object
    $median = $sorted[[int]($sorted.Count / 2)]
    $min = $sorted[0]
    "{0,-16} median {1,7:N2} ms   min {2,7:N2} ms" -f $c.Name, $median, $min | Write-Host
}
