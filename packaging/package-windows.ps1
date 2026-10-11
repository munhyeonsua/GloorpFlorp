<#
.SYNOPSIS
    Assembles the ready-to-run GloorpFlorp folder that goes into the Windows zip.

.DESCRIPTION
    Output layout (OutDir must be a folder named "GloorpFlorp"):

        GloorpFlorp.exe
        ffmpeg\ffmpeg.exe
        mpv\mpv.exe (+ any DLLs that sit next to it in the mpv build)
        licenses\...
        START HERE.txt
        GloorpFlorp Guide.html      (README.md as a web page)
        VERSIONS.txt
        THIRD-PARTY-NOTICES.txt

    Used by .github/workflows/build.yml. Also runs locally with PowerShell 7
    and 7-Zip ("7z") on PATH, e.g. to test the layout on Linux.
#>
#Requires -Version 7.0
[CmdletBinding()]
param(
    [Parameter(Mandatory)] [string] $AppExe,
    [Parameter(Mandatory)] [string] $FfmpegZip,
    [Parameter(Mandatory)] [string] $FfmpegUrl,
    [Parameter(Mandatory)] [string] $MpvArchive,
    [Parameter(Mandatory)] [string] $MpvRelease,
    [Parameter(Mandatory)] [string] $OutDir,
    [string] $MpvRepo = 'shinchiro/mpv-winbuild-cmake',
    [string] $Commit = 'unknown',
    [string] $Ref = 'local build',
    [string] $RepoUrl = ''
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$PackagingDir = $PSScriptRoot
$RepoRoot = Split-Path $PackagingDir -Parent

function Fail([string] $Message) {
    # "::error::" shows up as a red annotation on the GitHub Actions run page.
    Write-Host "::error::$Message"
    exit 1
}

# Writes text with Windows line endings (CRLF) and UTF-8 without BOM, so it
# reads well in Notepad.
function Write-WindowsText([string] $Path, [string[]] $Lines) {
    $text = (($Lines -join "`n") -replace "`r?`n", "`r`n") + "`r`n"
    [IO.File]::WriteAllText($Path, $text, [Text.UTF8Encoding]::new($false))
}

function Get-7Zip {
    $cmd = Get-Command 7z -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    $default = 'C:\Program Files\7-Zip\7z.exe'
    if (Test-Path -LiteralPath $default) { return $default }
    Fail '7-Zip (7z) was not found. Install 7-Zip and make sure "7z" is on PATH.'
}

function Expand-Download([string] $Archive, [string] $Destination, [string] $What) {
    New-Item -ItemType Directory -Force -Path $Destination | Out-Null
    & $SevenZip x $Archive "-o$Destination" -y -bso0 -bsp0
    if ($LASTEXITCODE -ne 0) {
        Fail "Could not extract the $What download ($Archive). The file is probably incomplete or corrupt; re-run the workflow."
    }
}

function Find-SingleFile([string] $Root, [string] $Name, [string] $What) {
    $found = @(Get-ChildItem -LiteralPath $Root -Recurse -File -Filter $Name)
    if ($found.Count -eq 0) {
        Fail "$Name was not found inside the $What download. Its layout may have changed upstream; update packaging/package-windows.ps1."
    }
    if ($found.Count -gt 1) {
        Fail "Found $($found.Count) copies of $Name inside the $What download ($($found.FullName -join ', ')). Update packaging/package-windows.ps1 to pick the right one."
    }
    return $found[0]
}

# Copies LICENSE*/COPYING*/Copyright* files found anywhere in an extracted
# download into licenses\, prefixed with the program name.
function Copy-LicenseFile([string] $Root, [string] $Prefix) {
    $files = @(Get-ChildItem -LiteralPath $Root -Recurse -File |
        Where-Object { $_.Name -match '^(LICEN[CS]E|COPYING|Copyright)' })
    foreach ($f in $files) {
        Copy-Item -LiteralPath $f.FullName -Destination (Join-Path $LicensesDir "$Prefix-$($f.Name)")
    }
    return $files.Count
}

# First line printed by `exe args`, or $null if it fails or takes over 30 s
# (a hung probe must not stall the build until the job timeout).
function Get-FirstLine([string] $Exe, [string[]] $Arguments) {
    try {
        $psi = [Diagnostics.ProcessStartInfo]::new($Exe)
        foreach ($a in $Arguments) { $psi.ArgumentList.Add($a) }
        $psi.UseShellExecute = $false
        $psi.RedirectStandardOutput = $true
        $p = [Diagnostics.Process]::Start($psi)
        $stdout = $p.StandardOutput.ReadToEndAsync()
        if (-not $p.WaitForExit(30000)) {
            $p.Kill($true)
            return $null
        }
        if ($p.ExitCode -ne 0 -or -not $stdout.Wait(5000)) { return $null }
        return @($stdout.Result -split "`r?`n" | Where-Object { $_.Trim() }) | Select-Object -First 1
    } catch {
        return $null
    }
}

# --- Check inputs --------------------------------------------------------

if (-not (Test-Path -LiteralPath $AppExe -PathType Leaf)) {
    Fail "GloorpFlorp.exe was not found at '$AppExe'. Did 'cargo build --release -p gloorpflorp' succeed?"
}
if (-not (Test-Path -LiteralPath $FfmpegZip -PathType Leaf)) {
    Fail "The FFmpeg download is missing ('$FfmpegZip'). Check the 'Download FFmpeg' step."
}
if (-not (Test-Path -LiteralPath $MpvArchive -PathType Leaf)) {
    Fail "The mpv download is missing ('$MpvArchive'). Check the 'Download mpv' step."
}
if (-not (Test-Path -LiteralPath (Join-Path $RepoRoot 'README.md') -PathType Leaf)) {
    Fail "README.md was not found in $RepoRoot; it becomes 'GloorpFlorp Guide.html' in the package."
}
if ((Split-Path $OutDir -Leaf) -ne 'GloorpFlorp') {
    # Guards the Remove-Item below and keeps the folder name users see stable.
    Fail "OutDir must be a folder named 'GloorpFlorp' (got '$OutDir')."
}

$SevenZip = Get-7Zip
$tempRoot = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [IO.Path]::GetTempPath() }
$Work = Join-Path $tempRoot "gloorpflorp-package-$([guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Force -Path $Work | Out-Null

if (Test-Path -LiteralPath $OutDir) { Remove-Item -LiteralPath $OutDir -Recurse -Force }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
# Absolute from here on: .NET calls below don't know PowerShell's current folder.
$OutDir = (Resolve-Path -LiteralPath $OutDir).Path
$LicensesDir = Join-Path $OutDir 'licenses'
New-Item -ItemType Directory -Force -Path $LicensesDir | Out-Null

try {
    # --- App -------------------------------------------------------------

    Copy-Item -LiteralPath $AppExe -Destination (Join-Path $OutDir 'GloorpFlorp.exe')

    # --- FFmpeg ----------------------------------------------------------

    Write-Host "Unpacking FFmpeg from $FfmpegZip"
    $ffWork = Join-Path $Work 'ffmpeg'
    Expand-Download $FfmpegZip $ffWork 'FFmpeg'
    $ffSource = Find-SingleFile $ffWork 'ffmpeg.exe' 'FFmpeg'
    $ffDir = Join-Path $OutDir 'ffmpeg'
    New-Item -ItemType Directory -Force -Path $ffDir | Out-Null
    $ffExe = Join-Path $ffDir 'ffmpeg.exe'
    Copy-Item -LiteralPath $ffSource.FullName -Destination $ffExe
    if ((Copy-LicenseFile $ffWork 'FFmpeg') -eq 0) {
        Fail 'No license file (LICENSE*/COPYING*) was found in the FFmpeg download. FFmpeg is GPL software and must ship with its license; check the download.'
    }

    $ffVersion = '(not checked: packaged on a non-Windows machine)'
    if ($IsWindows) {
        $ffVersion = Get-FirstLine $ffExe @('-hide_banner', '-version')
        if (-not $ffVersion) { Fail "The downloaded ffmpeg.exe does not run ('ffmpeg -version' failed)." }
    }

    # --- mpv -------------------------------------------------------------

    Write-Host "Unpacking mpv from $MpvArchive"
    $mpvWork = Join-Path $Work 'mpv'
    Expand-Download $MpvArchive $mpvWork 'mpv'
    $mpvSource = Find-SingleFile $mpvWork 'mpv.exe' 'mpv'
    $mpvDir = Join-Path $OutDir 'mpv'
    New-Item -ItemType Directory -Force -Path $mpvDir | Out-Null
    Copy-Item -LiteralPath $mpvSource.FullName -Destination $mpvDir
    $dlls = @(Get-ChildItem -LiteralPath $mpvSource.DirectoryName -File -Filter '*.dll')
    foreach ($dll in $dlls) { Copy-Item -LiteralPath $dll.FullName -Destination $mpvDir }
    Write-Host "mpv: copied mpv.exe and $($dlls.Count) DLL(s)"
    $mpvLicenses = Copy-LicenseFile $mpvWork 'mpv'
    Write-Host "mpv: found $mpvLicenses license file(s) in the download"

    $mpvVersion = '(not checked: packaged on a non-Windows machine)'
    if ($IsWindows) {
        # mpv.com is the console front end of mpv.exe; it prints to our pipe reliably.
        $mpvCom = Join-Path $mpvSource.DirectoryName 'mpv.com'
        $probe = if (Test-Path -LiteralPath $mpvCom) { $mpvCom } else { Join-Path $mpvDir 'mpv.exe' }
        $mpvVersion = Get-FirstLine $probe @('--no-config', '--version')
        if (-not $mpvVersion) {
            Write-Host "::warning::Could not read the mpv version ('mpv --version' failed). The package is still built; test the viewer on a real PC."
            $mpvVersion = '(unknown: mpv --version failed on the build machine)'
        } elseif ($mpvVersion -match '^mpv v?(\d+)\.(\d+)' -and [int]$Matches[1] -eq 0 -and [int]$Matches[2] -lt 41) {
            # The viewer passes --hwdec-threads, which older mpv refuses (it wouldn't start).
            Fail "The downloaded mpv is too old ($mpvVersion). GloorpFlorp's viewer needs mpv 0.41 or newer; check the 'Download mpv' step."
        }
    }

    # GPL v2 text for mpv (GPL v2 or later); FFmpeg's own license file is GPL v3.
    Copy-Item -LiteralPath (Join-Path $PackagingDir 'licenses/GPL-2.0.txt') -Destination $LicensesDir

    # --- Text files ------------------------------------------------------

    $mpvAsset = Split-Path $MpvArchive -Leaf
    $source = if ($RepoUrl) { "$RepoUrl/commit/$Commit" } else { $Commit }
    Write-WindowsText (Join-Path $OutDir 'VERSIONS.txt') @(
        'GloorpFlorp'
        "  Built from:  $source"
        "  Branch/tag:  $Ref"
        "  Built at:    $([DateTime]::UtcNow.ToString('yyyy-MM-dd HH:mm')) UTC"
        ''
        'FFmpeg (ffmpeg\ffmpeg.exe)'
        "  Version:     $ffVersion"
        "  Downloaded:  $FfmpegUrl"
        ''
        'mpv (mpv\mpv.exe)'
        "  Version:     $mpvVersion"
        "  Downloaded:  $mpvAsset from release $MpvRelease of https://github.com/$MpvRepo"
        "               https://github.com/$MpvRepo/releases/tag/$MpvRelease"
        ''
        'You and your friend should use the same GloorpFlorp build.'
    )

    $startHere = Get-Content -LiteralPath (Join-Path $PackagingDir 'start-here.txt')
    Write-WindowsText (Join-Path $OutDir 'START HERE.txt') $startHere

    # Rust crates compiled into GloorpFlorp.exe, with their licenses.
    Push-Location $RepoRoot
    try {
        $tree = & cargo tree -p gloorpflorp --target x86_64-pc-windows-msvc -e normal,no-proc-macro --prefix none --format '{p}  [{l}]'
        if ($LASTEXITCODE -ne 0) { Fail "'cargo tree' failed; cannot list the libraries built into GloorpFlorp.exe." }
    } finally {
        Pop-Location
    }
    $crates = @($tree |
        ForEach-Object { ($_ -replace '\s*\(\*\)\s*$', '').Trim() } |
        Where-Object { $_ -and $_ -notmatch '^(gf-[a-z]+|gloorpflorp) v' } |
        Sort-Object -Unique)
    $notices = @(Get-Content -LiteralPath (Join-Path $PackagingDir 'third-party-notices.txt')) + @('') + @($crates | ForEach-Object { "  $_" })
    Write-WindowsText (Join-Path $OutDir 'THIRD-PARTY-NOTICES.txt') $notices

    # README.md as a self-contained web page, readable with a double-click.
    $readme = Join-Path $RepoRoot 'README.md'
    $body = (ConvertFrom-Markdown -LiteralPath $readme).Html
    $css = Get-Content -LiteralPath (Join-Path $PackagingDir 'guide.css') -Raw
    $html = @(
        '<!doctype html>'
        '<html lang="en">'
        '<head>'
        '<meta charset="utf-8">'
        '<meta name="viewport" content="width=device-width, initial-scale=1">'
        '<title>GloorpFlorp Guide</title>'
        "<style>`n$css</style>"
        '</head>'
        '<body>'
        '<main>'
        $body
        '</main>'
        '</body>'
        '</html>'
    )
    Write-WindowsText (Join-Path $OutDir 'GloorpFlorp Guide.html') $html

    # --- Check the result ------------------------------------------------

    $required = @(
        'GloorpFlorp.exe', 'ffmpeg/ffmpeg.exe', 'mpv/mpv.exe',
        'START HERE.txt', 'GloorpFlorp Guide.html', 'VERSIONS.txt', 'THIRD-PARTY-NOTICES.txt',
        'licenses/GPL-2.0.txt'
    )
    foreach ($r in $required) {
        if (-not (Test-Path -LiteralPath (Join-Path $OutDir $r) -PathType Leaf)) {
            Fail "Packaging bug: '$r' is missing from $OutDir."
        }
    }

    $all = @(Get-ChildItem -LiteralPath $OutDir -Recurse -File | Sort-Object FullName)
    $totalMb = [math]::Round((($all | Measure-Object Length -Sum).Sum) / 1MB, 1)
    Write-Host "Package contents ($($all.Count) files, $totalMb MB before zipping):"
    foreach ($f in $all) {
        $rel = [IO.Path]::GetRelativePath($OutDir, $f.FullName)
        Write-Host ('  {0,10:N0}  {1}' -f $f.Length, $rel)
    }

    if ($env:GITHUB_STEP_SUMMARY) {
        $summary = @(
            '## GloorpFlorp for Windows'
            ''
            'Download **GloorpFlorp-windows-x64** from the *Artifacts* section at the bottom of this page, extract the whole zip, and open `START HERE.txt`.'
            ''
            '```'
        ) + @(Get-Content -LiteralPath (Join-Path $OutDir 'VERSIONS.txt')) + @('```')
        Add-Content -LiteralPath $env:GITHUB_STEP_SUMMARY -Value $summary
    }
} finally {
    Remove-Item -LiteralPath $Work -Recurse -Force -ErrorAction SilentlyContinue
}
