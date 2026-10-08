param([string]$Bundle = $PSScriptRoot)

$ErrorActionPreference = 'Stop'
$taskAgent = Join-Path $Bundle 'openlogi-agent.exe'
$taskDesktop = Join-Path $Bundle 'openlogi-desktop.exe'
$taskOverlay = Join-Path $Bundle 'openlogi-overlay.exe'
foreach ($taskBinary in @($taskAgent, $taskDesktop, $taskOverlay)) {
    if (!(Test-Path -LiteralPath $taskBinary -PathType Leaf)) {
        throw "Missing bundled binary: $taskBinary"
    }
}

# A different agent may hold a recovery journal. Never kill it from the launcher.
$taskOther = Get-CimInstance Win32_Process | Where-Object {
    $_.Name -eq 'openlogi-agent.exe' -and $_.ExecutablePath -ne $taskAgent
}
if ($taskOther) {
    throw 'Another OpenLogi agent is running. Restore its mouse profile and quit it before starting this bundle.'
}

function Start-MacroProcess([string]$TaskExecutable) {
    $taskStart = [System.Diagnostics.ProcessStartInfo]::new()
    $taskStart.FileName = $TaskExecutable
    $taskStart.WorkingDirectory = $Bundle
    $taskStart.UseShellExecute = $false
    $taskStart.CreateNoWindow = $true
    $taskStart.EnvironmentVariables['OPENLOGI_PROFILE'] = 'dev'
    [System.Diagnostics.Process]::Start($taskStart) | Out-Null
}

if (!(Get-CimInstance Win32_Process | Where-Object { $_.ExecutablePath -eq $taskAgent })) {
    Start-MacroProcess $taskAgent
}
Start-MacroProcess $taskDesktop
