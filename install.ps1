# MachineStream MCP installer for Windows.
#
#   irm https://machinestream.io/install.ps1 | iex
#
# Downloads the machinestream-mcp binary for this machine from the project's GitHub
# releases, checks it against the release's SHA-256 checksums, installs it for the current
# user (no administrator rights), adds it to the user's PATH and runs
# `machinestream-mcp setup`. Safe to run again: a rerun upgrades.
#
#   $env:MACHINESTREAM_VERSION      a version to install instead of the latest (0.1.0 or v0.1.0)
#   $env:MACHINESTREAM_INSTALL_DIR  where to install (default: %LOCALAPPDATA%\MachineStream\bin)
#   $env:MACHINESTREAM_NO_SETUP=1   install only, do not run setup
#   $env:MACHINESTREAM_DRY_RUN=1    say what would be done, and do nothing

# Everything runs in a scope of its own, so nothing here changes the session it was piped into.
& {
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'

    $Repo = 'contextstream/machinestream-mcp'
    $Name = 'machinestream-mcp'
    $Sums = 'SHA256SUMS'

    function Write-Row([string]$Label, [string]$Value) {
        Write-Host ('  {0,-10} {1}' -f $Label, $Value)
    }

    # The release asset for a CPU architecture, as Windows names it. Windows on ARM runs the x64 binary.
    function Get-MachineStreamAsset([string]$Architecture) {
        switch ($Architecture.ToUpperInvariant()) {
            'AMD64' { return "$Name-windows-x64.exe" }
            'X64'   { return "$Name-windows-x64.exe" }
            'ARM64' { return "$Name-windows-x64.exe" }
            default { throw "there is no binary for Windows on $Architecture yet. Build from source: https://github.com/$Repo" }
        }
    }

    # Where a version's files are: the latest release, or a named one.
    function Get-MachineStreamReleaseBase {
        if ($env:MACHINESTREAM_RELEASE_BASE_URL) {
            return $env:MACHINESTREAM_RELEASE_BASE_URL.TrimEnd('/')
        }
        if ($env:MACHINESTREAM_VERSION) {
            return "https://github.com/$Repo/releases/download/v$($env:MACHINESTREAM_VERSION.TrimStart('v'))"
        }
        return "https://github.com/$Repo/releases/latest/download"
    }

    function Install-MachineStream {
        $architecture = $env:MACHINESTREAM_ARCH
        if (-not $architecture) { $architecture = $env:PROCESSOR_ARCHITEW6432 }
        if (-not $architecture) { $architecture = $env:PROCESSOR_ARCHITECTURE }
        if (-not $architecture) { $architecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString() }
        $asset = Get-MachineStreamAsset $architecture
        $base = Get-MachineStreamReleaseBase
        $dir = $env:MACHINESTREAM_INSTALL_DIR
        if (-not $dir) { $dir = Join-Path $env:LOCALAPPDATA 'MachineStream\bin' }
        $target = Join-Path $dir "$Name.exe"
        $version = if ($env:MACHINESTREAM_VERSION) { $env:MACHINESTREAM_VERSION.TrimStart('v') } else { 'latest' }

        Write-Host 'MachineStream MCP installer'
        Write-Host ''
        Write-Row 'Platform' ($asset -replace "^$Name-", '' -replace '\.exe$', '')
        Write-Row 'Version' $version

        if ($env:MACHINESTREAM_DRY_RUN -eq '1') {
            Write-Row 'Download' "$base/$asset"
            Write-Row 'Checksums' "$base/$Sums"
            Write-Row 'Install' $target
            Write-Host ''
            Write-Host 'Dry run: nothing was downloaded or changed.'
            return
        }

        [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
        $tmp = Join-Path ([IO.Path]::GetTempPath()) ("machinestream-" + [Guid]::NewGuid().ToString('N'))
        New-Item -ItemType Directory -Path $tmp | Out-Null
        $staged = $null
        try {
            $download = Join-Path $tmp $asset
            $sumsFile = Join-Path $tmp $Sums
            Invoke-WebRequest -UseBasicParsing -Uri "$base/$asset" -OutFile $download
            Invoke-WebRequest -UseBasicParsing -Uri "$base/$Sums" -OutFile $sumsFile
            Write-Row 'Download' $asset

            $expected = $null
            foreach ($line in Get-Content -LiteralPath $sumsFile) {
                $parts = $line.Trim() -split '\s+', 2
                if ($parts.Count -eq 2 -and $parts[1].TrimStart('*') -eq $asset) {
                    $expected = $parts[0].ToLowerInvariant()
                    break
                }
            }
            if (-not $expected) { throw "$Sums does not list $asset" }
            $actual = (Get-FileHash -LiteralPath $download -Algorithm SHA256).Hash.ToLowerInvariant()
            if ($actual -ne $expected) { throw "the download did not match its checksum (expected $expected, got $actual)" }
            Write-Row 'Verified' ("sha256 " + $actual.Substring(0, 16))

            # Staged beside the target and renamed, so a half-written binary is never left behind.
            New-Item -ItemType Directory -Force -Path $dir | Out-Null
            $staged = Join-Path $dir (".$Name." + $PID + '.tmp')
            Copy-Item -LiteralPath $download -Destination $staged -Force
            try {
                Move-Item -LiteralPath $staged -Destination $target -Force
            } catch {
                throw "could not replace $target (close any program that is running it, then try again)"
            }
            $staged = $null
        } finally {
            if ($staged -and (Test-Path -LiteralPath $staged)) { Remove-Item -LiteralPath $staged -Force }
            Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
        }

        $installed = & $target --version
        if (-not $installed) { throw "$target does not run on this machine" }
        Write-Row 'Installed' "$target ($installed)"

        # Add the folder to the user's PATH (no administrator rights needed), and to this session's.
        $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
        $folders = @()
        if ($userPath) { $folders = $userPath -split ';' | Where-Object { $_ } }
        if ($folders -notcontains $dir) {
            [Environment]::SetEnvironmentVariable('Path', (($folders + $dir) -join ';'), 'User')
            Write-Host ''
            Write-Host "  Added $dir to your PATH. Open a new terminal for other programs to see it."
        }
        if (($env:Path -split ';') -notcontains $dir) { $env:Path = "$env:Path;$dir" }

        Write-Host ''
        $interactive = [Environment]::UserInteractive -and -not [Console]::IsInputRedirected
        if ($env:MACHINESTREAM_NO_SETUP -eq '1') {
            Write-Host "Next: $Name setup"
        } elseif ($interactive) {
            & $target setup
            if ($LASTEXITCODE -ne 0) { Write-Host "Setup did not finish. Run it again any time: $Name setup" }
        } else {
            Write-Host 'There is no terminal to ask on, so setup was not run.'
            Write-Host "Next: $Name setup"
        }
    }

    try {
        Install-MachineStream
    } catch {
        Write-Host ''
        Write-Host "machinestream-mcp was not installed: $($_.Exception.Message)" -ForegroundColor Red
        # Run as a file, the failure is the exit status. Run through `iex`, exiting would close
        # the person's own PowerShell window, so the line above is all there is.
        if ($PSCommandPath) { exit 1 }
    }
}
