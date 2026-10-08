param([string]$OutputDirectory)

$ErrorActionPreference = 'Stop'
$taskRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
if (!$OutputDirectory) {
    $OutputDirectory = Join-Path $taskRoot 'target\packages'
}
$OutputDirectory = [System.IO.Path]::GetFullPath($OutputDirectory)

Push-Location $taskRoot
try {
    $taskStatus = & git status --porcelain
    if ($LASTEXITCODE -ne 0 -or $taskStatus) {
        throw 'Commit all source changes before building a reproducible package.'
    }
    $taskCommit = & git rev-parse HEAD
    if ($LASTEXITCODE -ne 0) { throw 'Cannot read the source commit.' }
    $taskBranch = & git branch --show-current
    if ($LASTEXITCODE -ne 0) { throw 'Cannot read the source branch.' }

    & cargo +1.98.0 build --release --locked -p openlogi-agent -p openlogi-desktop -p openlogi-overlay --bin openlogi-agent --bin openlogi-desktop --bin openlogi-overlay
    if ($LASTEXITCODE -ne 0) { throw 'Release build failed.' }
    & cargo +1.98.0 build --release --locked -p openlogi-agent --example g604_recovery
    if ($LASTEXITCODE -ne 0) { throw 'Recovery utility build failed.' }

    $taskBundleName = 'OpenLogi-G604-Macros-windows-x64-' + $taskCommit.Substring(0, 8)
    $taskBundle = Join-Path $OutputDirectory $taskBundleName
    if (Test-Path -LiteralPath $taskBundle) { throw "Package already exists: $taskBundle" }
    New-Item -ItemType Directory -Path $taskBundle -Force | Out-Null
    foreach ($taskBinary in @('openlogi-agent.exe', 'openlogi-desktop.exe', 'openlogi-overlay.exe')) {
        Copy-Item -LiteralPath (Join-Path $taskRoot "target\release\$taskBinary") -Destination $taskBundle
    }
    Copy-Item -LiteralPath (Join-Path $taskRoot 'target\release\examples\g604_recovery.exe') -Destination (Join-Path $taskBundle 'g604-recovery.exe')
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'start.ps1') -Destination $taskBundle
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'start.cmd') -Destination $taskBundle
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'restore.cmd') -Destination $taskBundle
    Copy-Item -LiteralPath (Join-Path $taskRoot 'docs\G604-BLUETOOTH-MACROS.md') -Destination (Join-Path $taskBundle 'README.md')
    foreach ($taskLicense in @('LICENSE-MIT', 'LICENSE-APACHE')) {
        Copy-Item -LiteralPath (Join-Path $taskRoot $taskLicense) -Destination $taskBundle
    }

    $taskVersion = [regex]::Match((Get-Content -LiteralPath (Join-Path $taskRoot 'Cargo.toml') -Raw), '(?m)^version = "([^"]+)"').Groups[1].Value
    $taskProtocol = [regex]::Match((Get-Content -LiteralPath (Join-Path $taskRoot 'crates\openlogi-ipc\src\ipc.rs') -Raw), 'PROTOCOL_VERSION: u32 = (\d+)').Groups[1].Value
    $taskSchema = [regex]::Match((Get-Content -LiteralPath (Join-Path $taskRoot 'crates\openlogi-core\src\config.rs') -Raw), 'SCHEMA_VERSION: u32 = (\d+)').Groups[1].Value
    if (!$taskVersion -or !$taskProtocol -or !$taskSchema) { throw 'Cannot read package versions.' }
    $taskFiles = @(Get-ChildItem -LiteralPath $taskBundle -File | Sort-Object Name | ForEach-Object {
        [ordered]@{ name = $_.Name; bytes = $_.Length; sha256 = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant() }
    })
    $taskManifest = [ordered]@{
        version = $taskVersion
        variant = 'g604-bluetooth-macros'
        protocol_version = [int]$taskProtocol
        config_schema = [int]$taskSchema
        build = 'release'
        target = 'x86_64-pc-windows-msvc'
        rust = '1.98.0'
        repository = 'https://github.com/inspirai-store/OpenLogi'
        source_commit = $taskCommit
        branch = $taskBranch
        generated_at_utc = [DateTime]::UtcNow.ToString('o')
        physical_trigger_validation = 'pending'
        files = $taskFiles
    }
    $taskManifest | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $taskBundle 'manifest.json') -Encoding UTF8
    $taskChecksums = $taskFiles | ForEach-Object { $_.sha256 + '  ' + $_.name }
    $taskChecksums | Set-Content -LiteralPath (Join-Path $taskBundle 'SHA256SUMS') -Encoding ASCII
    $taskZip = Join-Path $OutputDirectory ($taskBundleName + '.zip')
    Compress-Archive -LiteralPath $taskBundle -DestinationPath $taskZip
    ((Get-FileHash -LiteralPath $taskZip -Algorithm SHA256).Hash.ToLowerInvariant() + '  ' + [System.IO.Path]::GetFileName($taskZip)) | Set-Content -LiteralPath ($taskZip + '.sha256') -Encoding ASCII
    Write-Output "Package: $taskBundle"
    Write-Output "Archive: $taskZip"
} finally {
    Pop-Location
}
