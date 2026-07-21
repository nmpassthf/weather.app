Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

foreach ($name in @("TESTED_COMMIT", "SOURCE_RUN", "GITHUB_RUN_ID")) {
    if ([string]::IsNullOrWhiteSpace([Environment]::GetEnvironmentVariable($name))) {
        throw "$name is required"
    }
}

function Invoke-NativeCommand {
    param(
        [Parameter(Mandatory = $true)]
        [scriptblock]$Command,
        [Parameter(Mandatory = $true)]
        [string]$Description
    )

    & $Command
    if ($LASTEXITCODE -ne 0) {
        throw "$Description failed with exit code $LASTEXITCODE"
    }
}

function Write-ArtifactMetadata {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Directory,
        [Parameter(Mandatory = $true)]
        [string]$Target
    )

    [ordered]@{
        target = $Target
        commit = $env:TESTED_COMMIT
        source_run = $env:SOURCE_RUN
        bundle_run = $env:GITHUB_RUN_ID
    } | ConvertTo-Json | Set-Content -Encoding utf8NoBOM "$Directory/manifest.json"

    $checksums = Get-ChildItem $Directory -File |
        Where-Object Name -ne "SHA256SUMS" |
        Sort-Object Name |
        ForEach-Object {
            $hash = (Get-FileHash -Algorithm SHA256 $_.FullName).Hash.ToLower()
            "$hash  $($_.Name)"
        }
    $checksums | Set-Content -Encoding utf8NoBOM "$Directory/SHA256SUMS"
}

$previousLocation = Get-Location
$repositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
try {
    Set-Location $repositoryRoot

    Invoke-NativeCommand -Description "Bun installation" -Command {
        npm install --global bun@1.3.14
    }

    Push-Location weather-renderer/gui
    try {
        Invoke-NativeCommand -Description "Frontend dependency installation" -Command {
            bun install --frozen-lockfile
        }
        Invoke-NativeCommand -Description "Windows bundle build" -Command {
            bun run tauri build --features desktop --bundles nsis
        }
    }
    finally {
        Pop-Location
    }

    $files = @(Get-ChildItem target/release/bundle/nsis/*.exe)
    if ($files.Count -ne 1) {
        throw "expected exactly one NSIS installer"
    }
    if (-not (Test-Path target/release/weather.app.exe)) {
        throw "missing portable weather.app.exe"
    }

    New-Item -ItemType Directory -Force dist/windows-nsis | Out-Null
    New-Item -ItemType Directory -Force dist/windows-tar/payload | Out-Null
    Copy-Item $files[0].FullName dist/windows-nsis/
    Copy-Item target/release/weather.app.exe dist/windows-tar/payload/
    Invoke-NativeCommand -Description "Windows portable archive creation" -Command {
        tar -C dist/windows-tar/payload `
            -czf dist/windows-tar/weather-windows-x86_64.tar.gz weather.app.exe
    }
    Remove-Item -Recurse dist/windows-tar/payload

    Write-ArtifactMetadata dist/windows-nsis windows-x86_64-nsis
    Write-ArtifactMetadata dist/windows-tar windows-x86_64-tar
}
finally {
    Set-Location $previousLocation
}
