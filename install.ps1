# vexus installer for Windows.
#
#   irm https://raw.githubusercontent.com/faique43/vexus/main/install.ps1 | iex
#
# Downloads the release archive for this machine, verifies it against the
# release's SHA256SUMS, and installs vexus.exe to %LOCALAPPDATA%\vexus\bin
# (override with $env:VEXUS_INSTALL_DIR). Set $env:VEXUS_VERSION to pin a
# version instead of taking the latest.

$ErrorActionPreference = "Stop"

$repo = "faique43/vexus"
$installDir = if ($env:VEXUS_INSTALL_DIR) { $env:VEXUS_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "vexus\bin" }

function Fail($msg) {
    Write-Error "install.ps1: $msg"
    exit 1
}

$target = switch ($env:PROCESSOR_ARCHITECTURE) {
    "AMD64" { "x86_64-pc-windows-msvc" }
    "ARM64" { "aarch64-pc-windows-msvc" }
    default { Fail "no prebuilt binary for architecture '$($env:PROCESSOR_ARCHITECTURE)'" }
}

$version = $env:VEXUS_VERSION
if (-not $version) {
    try {
        $release = Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest"
        $version = $release.tag_name
    } catch {
        Fail "could not determine the latest release (rate limited? set `$env:VEXUS_VERSION)"
    }
}
$version = $version.TrimStart("v")

$name = "vexus-$version-$target"
$base = "https://github.com/$repo/releases/download/v$version"

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("vexus-install-" + [System.Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    Write-Host "vexus $version ($target)"
    Invoke-WebRequest -Uri "$base/$name.zip" -OutFile (Join-Path $tmp "$name.zip")
    Invoke-WebRequest -Uri "$base/SHA256SUMS" -OutFile (Join-Path $tmp "SHA256SUMS")

    # Verification is not optional: an unverified binary is worse than none.
    $sumsLine = Select-String -Path (Join-Path $tmp "SHA256SUMS") -Pattern ("\s" + [regex]::Escape("$name.zip") + "$") | Select-Object -First 1
    if (-not $sumsLine) { Fail "no checksum for $name.zip in SHA256SUMS" }
    $expected = ($sumsLine.Line -split "\s+")[0].ToLower()
    $actual = (Get-FileHash -Algorithm SHA256 (Join-Path $tmp "$name.zip")).Hash.ToLower()
    if ($expected -ne $actual) {
        Fail "checksum mismatch - refusing to install`n  expected $expected`n  actual   $actual"
    }

    Expand-Archive -Path (Join-Path $tmp "$name.zip") -DestinationPath $tmp
    New-Item -ItemType Directory -Force -Path $installDir | Out-Null
    Copy-Item (Join-Path $tmp "$name\vexus.exe") (Join-Path $installDir "vexus.exe") -Force

    Write-Host "installed $(Join-Path $installDir 'vexus.exe')"

    # Add the install dir to the *user* PATH if it isn't there — never the
    # machine PATH (no elevation) and never the volatile process PATH alone.
    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if (($userPath -split ";") -notcontains $installDir) {
        [Environment]::SetEnvironmentVariable("Path", "$userPath;$installDir", "User")
        $env:Path = "$env:Path;$installDir"
        Write-Host ""
        Write-Host "$installDir was added to your user PATH (new terminals pick it up automatically)."
    }

    Write-Host ""
    Write-Host "next, inside the repo you want indexed:"
    Write-Host "  vexus index .                    # build the index (first run downloads a ~160 MB model; large repos take minutes)"
    Write-Host "  vexus init --agent claude-code   # install the steering pack + register the MCP server in .mcp.json"
    Write-Host ""
    Write-Host "no need to run 'vexus serve' yourself - your agent launches it via .mcp.json"
} finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
