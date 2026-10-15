<#
.SYNOPSIS
  Builds a OneUpDrive release: the programs, then a Velopack installer and
  update packages, optionally signed and published to GitHub Releases.

.DESCRIPTION
  Needs: Rust (stable, MSVC), the .NET SDK and Velopack's packer
  (`dotnet tool install -g vpk`). Used by .github/workflows/release.yml;
  works the same on a developer's PC.

  Output in target\package\releases: OneUpDrive.App-<channel>-Setup.exe (the
  installer), the full and delta update packages, and the release feed.

.EXAMPLE
  # An unsigned local build, to try the installer on a test PC:
  .\packaging\build-release.ps1 -Version 0.2.0

.EXAMPLE
  # Signed with Azure Artifact Signing (Trusted Signing) and published:
  .\packaging\build-release.ps1 -Version 0.2.0 -Repo https://github.com/OWNER/oneupdrive `
      -AzureSignFile signing.json -Publish -Token $env:GITHUB_TOKEN
#>
param(
    [Parameter(Mandatory)] [string] $Version,
    # stable, or preview (published as a GitHub pre-release)
    [ValidateSet("stable", "preview")] [string] $Channel = "stable",
    # The GitHub repository updates come from (baked into the programs).
    [string] $Repo = "",
    # Sign with Azure Artifact Signing: the metadata file (see vpk's docs).
    [string] $AzureSignFile = "",
    # Or with signtool: its parameters, e.g. '/a /fd sha256 /tr http://timestamp.digicert.com /td sha256'.
    [string] $SignParams = "",
    [switch] $Publish,
    [string] $Token = "",
    # The signing certificate's subject, exactly (e.g. "CN=Jane Doe, O=Jane Doe, L=City, S=State, C=US"):
    # the identity package's publisher (Explorer integration). Unsigned builds use a development name.
    [string] $Publisher = "",
    # The app registration's Application (client) ID that people sign in through
    # (a public client: not a secret, but kept out of the repository).
    # Without it, nobody can sign in.
    [string] $ClientId = "",
    # Stop after building and staging (the workflow signs the staged files, then runs -SkipBuild).
    [switch] $StageOnly,
    # Start from what -StageOnly left.
    [switch] $SkipBuild
)
# Not "Stop": Windows PowerShell 5.1 would treat cargo's progress output (on
# stderr) as an error. Every program's exit code is checked instead.
$ErrorActionPreference = "Continue"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
if ($Version -notmatch '^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$') { throw "Version must look like 1.2.3 or 1.2.3-beta.1" }

$app = "target\package\app"
$out = "target\package\releases"
if (-not $Publisher) { $Publisher = "CN=OneUpDrive Development" }

if (-not $SkipBuild) {
# ---- the programs, with this version, the update repository and the publisher
$manifest = "bins\oneup\Cargo.toml"
$original = Get-Content $manifest -Raw
try {
    $versioned = [regex]::Replace($original, '(?m)^version = "[^"]*"', "version = `"$Version`"", 1)
    [IO.File]::WriteAllText((Join-Path $root $manifest), $versioned)
    $env:ONEUP_UPDATE_REPO = $Repo
    $env:ONEUP_PUBLISHER = $Publisher
    if ($ClientId) { $env:ONEUP_CLIENT_ID = $ClientId } else { Write-Warning "No -ClientId: this build can't sign anyone in." }
    cargo build --release -p oneup -p oneup-shellext
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
} finally {
    [IO.File]::WriteAllText((Join-Path $root $manifest), $original)
    Remove-Item Env:ONEUP_UPDATE_REPO, Env:ONEUP_PUBLISHER, Env:ONEUP_CLIENT_ID -ErrorAction SilentlyContinue
}

# ---- what gets installed
Remove-Item -Recurse -Force $app -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $app, $out | Out-Null
foreach ($exe in "oneup", "oneupd", "oneupd-start", "oneup-tray", "oneup-settings", "oneup-open") {
    Copy-Item "target\release\$exe.exe" $app
}
# Explorer's commands (loaded in a surrogate process; see docs/architecture/explorer.md).
Copy-Item "target\release\oneup_shell.dll" $app
Copy-Item LICENSE, THIRD-PARTY-NOTICES.txt $app -ErrorAction SilentlyContinue

# ---- the identity package (Explorer integration), next to the programs
$kit = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin\*\x64\makeappx.exe" | Sort-Object FullName | Select-Object -Last 1
if (-not $kit) { throw "makeappx.exe not found (Windows SDK)" }
$layout = "target\package\identity"
Remove-Item -Recurse -Force $layout -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force "$layout\Assets" | Out-Null
Copy-Item packaging\identity\Assets\*.png "$layout\Assets"
$parts = ($Version -split '[-+]')[0].Split('.') + @('0')
$pkgVersion = ($parts[0..3]) -join '.'
$xml = [Security.SecurityElement]::Escape($Publisher)
$m = (Get-Content packaging\identity\AppxManifest.xml -Raw).Replace("{{PUBLISHER}}", $xml).Replace("{{VERSION}}", $pkgVersion)
[IO.File]::WriteAllText((Join-Path $root "$layout\AppxManifest.xml"), $m)
& $kit.FullName pack /o /nv /d $layout /p "$app\OneUpDrive.Sync.msix" | Out-Null
if ($LASTEXITCODE -ne 0) { throw "makeappx failed" }

if ($StageOnly) { Write-Host "Staged in $app"; exit 0 }
}

# ---- the previous release, so updates can be small (deltas)
if ($Repo) {
    vpk download github --repoUrl $Repo --channel $Channel --outputDir $out @(if ($Token) { "--token", $Token })
    if ($LASTEXITCODE -ne 0) { Write-Warning "No earlier release found: this one has no delta update." }
}

# ---- installer and update packages (vpk signs every program first)
# The package id names the install folder (%LOCALAPPDATA%\<id>): never
# "OneUpDrive", which is the default data folder (database, sign-in, vault
# keys) that uninstalling would remove with the programs.
$pack = @(
    "pack", "--packId", "OneUpDrive.App", "--packVersion", $Version, "--packDir", $app,
    "--runtime", "win-x64", "--shortcuts", "StartMenuRoot",
    "--mainExe", "oneup-tray.exe", "--packTitle", "OneUpDrive", "--packAuthors", "OneUpDrive contributors",
    "--icon", "packaging\oneupdrive.ico", "--channel", $Channel, "--outputDir", $out
)
if ($AzureSignFile) { $pack += "--azureTrustedSignFile", $AzureSignFile }
elseif ($SignParams) { $pack += "--signParams", $SignParams }
else { Write-Warning "Not signed: the programs won't update themselves, and Windows will warn when installing." }
vpk @pack
if ($LASTEXITCODE -ne 0) { throw "vpk pack failed" }

# ---- publish
if ($Publish) {
    if (-not $Repo -or -not $Token) { throw "-Publish needs -Repo and -Token" }
    $up = @("upload", "github", "--repoUrl", $Repo, "--token", $Token, "--outputDir", $out, "--channel", $Channel,
            "--publish", "--releaseName", "OneUpDrive $Version", "--tag", "v$Version")
    if ($Channel -eq "preview") { $up += "--pre" }
    vpk @up
    if ($LASTEXITCODE -ne 0) { throw "vpk upload failed" }
}
Write-Host "Done: $out"
