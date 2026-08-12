$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot
$binary = Join-Path $repoRoot "target\debug\agentrt.exe"
$demoRoot = Join-Path $env:TEMP ("agentrt-demo-" + [guid]::NewGuid().ToString("N"))
$workspace = Join-Path $demoRoot "workspace"
$store = Join-Path $demoRoot "state.db"
$denialStore = Join-Path $demoRoot "denial.db"
$bundle = Join-Path $demoRoot "audit-bundle"
$stdout = Join-Path $demoRoot "agent.stdout.log"
$stderr = Join-Path $demoRoot "agent.stderr.log"
$runId = "demo-recovery-$PID"
$plan = Join-Path $repoRoot "fixtures\evals\model-plan\plan.json"

New-Item -ItemType Directory -Path $workspace -Force | Out-Null
Set-Content -LiteralPath (Join-Path $workspace "input.txt") -Value "status=broken`n" -NoNewline

Push-Location $repoRoot
try {
    cargo build --offline | Out-Host
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

    $arguments = @(
        "agent", "repo-fix-model",
        "--workspace", $workspace,
        "--prompt", "repair-fixture",
        "--response-file", $plan,
        "--store", $store,
        "--run-id", $runId,
        "--pause-ms", "3000"
    )
    $process = Start-Process -FilePath $binary -ArgumentList $arguments -RedirectStandardOutput $stdout -RedirectStandardError $stderr -PassThru -WindowStyle Hidden

    $resultDurable = $false
    for ($attempt = 0; $attempt -lt 100; $attempt++) {
        if (Test-Path -LiteralPath $store) {
            $audit = & $binary audit --store $store --run-id $runId 2>$null
            if ($audit -match "tool.result") {
                $resultDurable = $true
                break
            }
        }
        Start-Sleep -Milliseconds 50
    }
    if (-not $resultDurable) { throw "timed out waiting for a durable tool result" }

    Stop-Process -Id $process.Id -Force
    $process.WaitForExit()
    & $binary resume --store $store --run-id $runId | Out-Host
    if ($LASTEXITCODE -ne 0) { throw "resume failed" }

    & $binary audit --store $store --run-id $runId --export $bundle | Out-Host
    if ($LASTEXITCODE -ne 0) { throw "audit export failed" }
    $events = Get-Content -LiteralPath (Join-Path $bundle "events.jsonl") -Raw
    if ($events -notmatch "tool.deduplicated") { throw "audit did not prove deduplication" }
    if ($events -notmatch "llm.request" -or $events -notmatch "agent.plan") { throw "audit did not prove model planning" }

    & $binary tool write --workspace $workspace --path denied.txt --contents blocked --store $denialStore --read-only | Out-Host
    if ($LASTEXITCODE -eq 0) { throw "sandbox denial unexpectedly succeeded" }

    & $binary eval --break | Out-Host
    if ($LASTEXITCODE -eq 0) { throw "intentional regression unexpectedly passed" }

    Write-Host "demo=passed"
    Write-Host "run_id=$runId"
    Write-Host "audit_bundle=$bundle"
}
finally {
    Pop-Location
}
