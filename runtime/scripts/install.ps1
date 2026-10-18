# SPDX-License-Identifier: Apache-2.0
# Install Palette on Windows. Palette runs in WSL2; this sets WSL2 up, installs
# Palette inside it, and connects Claude Desktop, Claude Code and Codex:
#   powershell -c "iex ((New-Object Net.WebClient).DownloadString('https://github.com/latentcollapse/Palette.jl/releases/latest/download/install.ps1'))"
# Run again to update. Settings, all optional:
#   PALETTE_WSL_DISTRIBUTION  WSL distribution to use (default: the default one, or a new Ubuntu)
#   PALETTE_VERSION           release tag to install (default: the latest release)
#   PALETTE_RELEASE_URL       download the release from this base URL instead
& {
$ErrorActionPreference = 'Stop'
$env:WSL_UTF8 = '1'
[Console]::OutputEncoding = New-Object Text.UTF8Encoding $false

function Say($text) { Write-Host "==> $text" }
function Stop-Install($text) { throw "Palette install stopped: $text" }

if (-not (Get-Command wsl.exe -ErrorAction SilentlyContinue)) {
    Stop-Install 'this Windows has no WSL; Palette needs Windows 10 version 2004 or newer, or Windows 11'
}
$release = $env:PALETTE_RELEASE_URL
if (-not $release) {
    $release = if ($env:PALETTE_VERSION) { "https://github.com/latentcollapse/Palette.jl/releases/download/$env:PALETTE_VERSION" }
               else { 'https://github.com/latentcollapse/Palette.jl/releases/latest/download' }
}

# Linux commands; their output is UTF-8 and wsl.exe's own messages are too.
# Arguments holding spaces must not also hold double quotes: Windows
# PowerShell 5.1 passes those to wsl.exe unescaped.
function In-Linux { & wsl.exe --distribution $distribution @args }

# Windows PowerShell stops on a native command's redirected stderr under
# ErrorActionPreference Stop, so silenced commands run under Continue.
function Get-ExitCode([string]$exe, [string[]]$arguments) {
    $ErrorActionPreference = 'Continue'
    & $exe @arguments *> $null
    $LASTEXITCODE
}

function Find-Distribution {
    if ($env:PALETTE_WSL_DISTRIBUTION) { return $env:PALETTE_WSL_DISTRIBUTION }
    $ErrorActionPreference = 'Continue'
    $name = (& wsl.exe --exec sh -c 'echo $WSL_DISTRO_NAME' 2>$null | Out-String).Trim()
    # Docker Desktop's own distributions have no package manager to install into.
    if ($LASTEXITCODE -eq 0 -and $name -and $name -notlike 'docker-desktop*') { return $name }
    return $null
}

$distribution = Find-Distribution
if (-not $distribution) {
    Say 'Installing WSL2 with Ubuntu (Windows asks for permission; Ubuntu asks for a new Linux user name and password)'
    & wsl.exe --install --distribution Ubuntu
    $distribution = Find-Distribution
    if (-not $distribution) {
        Write-Host 'WSL2 is installed but not ready yet. Restart Windows, open Ubuntu from the Start menu once to create your Linux user, then run this command again.'
        return
    }
}
$kernel = (In-Linux --exec uname -r | Out-String).Trim()
if ($LASTEXITCODE -ne 0) { Stop-Install "WSL distribution '$distribution' does not start" }
if ($kernel -notmatch 'WSL2') {
    Stop-Install "'$distribution' runs under WSL1, which cannot run the sandbox. Convert it with: wsl --set-version $distribution 2"
}
Say "Using WSL distribution $distribution"

# Root inside WSL needs no password, so the sandbox's system packages are
# installed here rather than by sudo inside install.sh.
Say 'Installing the sandbox system packages'
In-Linux --user root --exec sh -c 'command -v curl >/dev/null || { apt-get update && apt-get install -y curl ca-certificates; }; curl -fsSL $0/setup-sandbox-host.sh | sh -s install' $release
if ($LASTEXITCODE -ne 0) { Stop-Install 'could not install the sandbox system packages' }

# The Linux installer reads its settings from these, passed through WSLENV.
$env:PALETTE_WSL_DISTRIBUTION = $distribution
$env:PALETTE_RELEASE_URL = $release
$env:WSLENV = (@('PALETTE_WSL_DISTRIBUTION', 'PALETTE_RELEASE_URL', 'PALETTE_HOME') + @($env:WSLENV -split ':' | Where-Object { $_ })) -join ':'
In-Linux --exec sh -c 'curl -fsSL $0/install.sh | sh' $release
if ($LASTEXITCODE -ne 0) { Stop-Install 'the Linux installer did not finish; its message is above' }

$launch = (In-Linux --exec sh -c 'cat ${PALETTE_HOME:-${XDG_DATA_HOME:-$HOME/.local/share}/palette}/client/palette-launch.json' | Out-String) | ConvertFrom-Json
if ($launch.command -ne 'wsl.exe') { Stop-Install 'the Linux installer did not write a Windows launch configuration' }

# Claude Desktop reads its servers from one JSON file; add Palette, keep the rest.
$desktopDir = Join-Path $env:APPDATA 'Claude'
if (Test-Path $desktopDir) {
    $desktopFile = Join-Path $desktopDir 'claude_desktop_config.json'
    $raw = if (Test-Path $desktopFile) { Get-Content -Raw $desktopFile } else { $null }
    $config = if ($raw -and $raw.Trim()) { $raw | ConvertFrom-Json } else { $null }
    if (-not $config) { $config = New-Object PSObject }
    if (-not $config.PSObject.Properties['mcpServers']) { $config | Add-Member mcpServers (New-Object PSObject) }
    if ($config.mcpServers.PSObject.Properties['palette']) {
        Write-Host "Claude Desktop already has a server named palette; left as it is. This install's entry is in the Linux file client/palette-launch.json."
    } else {
        if (Test-Path $desktopFile) { Copy-Item $desktopFile "$desktopFile.before-palette" -Force }
        $config.mcpServers | Add-Member palette $launch
        [IO.File]::WriteAllText($desktopFile, ($config | ConvertTo-Json -Depth 32), (New-Object Text.UTF8Encoding $false))
        Write-Host 'Registered with Claude Desktop; restart it to load Palette.'
    }
}

# Claude Code and Codex take the same command line. An npm install puts a
# .ps1 shim first on PATH, which the default execution policy blocks, so the
# .cmd or .exe beside it is run instead.
foreach ($client in @(@{ Name = 'Claude Code'; Exe = 'claude'; Add = @('mcp', 'add', '--scope', 'user', 'palette') },
                      @{ Name = 'Codex'; Exe = 'codex'; Add = @('mcp', 'add', 'palette') })) {
    $exe = Get-Command $client.Exe -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if (-not $exe) { continue }
    $add = $client.Add + @('--', $launch.command) + @($launch.args)
    if ((Get-ExitCode $exe.Source @('mcp', 'get', 'palette')) -eq 0) {
        Write-Host "$($client.Name) already has a server named palette; left as it is. To use this one: $($client.Exe) mcp remove palette, then $($client.Exe) $($add -join ' ')"
        continue
    }
    if ((Get-ExitCode $exe.Source $add) -eq 0) { Write-Host "Registered with $($client.Name)." }
    else { Write-Host "Could not register with $($client.Name); run: $($client.Exe) $($add -join ' ')" }
}

Write-Host ''
Write-Host 'Palette is installed. Start a new Claude session and ask it to evaluate x = 41, then x + 1.'
Write-Host "Other clients: run '$($launch.command) $($launch.args -join ' ')' as a stdio MCP server."
}
