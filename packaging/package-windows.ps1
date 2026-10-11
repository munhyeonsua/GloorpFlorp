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

    licenses\SOURCES.txt lists the exact FFmpeg and mpv commits and the
    build-script releases in a key=value form; the release job in build.yml
    reads it to attach those source archives to the GitHub Release.

    Used by .github/workflows/build.yml. Also runs locally with PowerShell 7
    and 7-Zip ("7z") on PATH, e.g. to test the layout on Linux.

.PARAMETER FfmpegRelease
    The dated BtbN release the FFmpeg zip came from (e.g.
    autobuild-2026-10-10-13-04). 'unknown' (the default, e.g. for local
    runs) links to BtbN's repository in general instead.

.PARAMETER RefType
    'tag' for a tag build (GITHUB_REF_TYPE). Only a tag build named like
    v1.0.0 says in THIRD-PARTY-NOTICES.txt that the source code is attached
    to a GitHub release, because only tags are published (see build.yml).
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
    [string] $FfmpegRelease = 'unknown',
    [string] $FfmpegRepo = 'BtbN/FFmpeg-Builds',
    [string] $MpvRepo = 'shinchiro/mpv-winbuild-cmake',
    [string] $Commit = 'unknown',
    [string] $Ref = 'local build',
    [string] $RefType = 'branch',
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

# The non-empty lines printed by `exe args`, or $null if it fails or takes
# over 30 s (a hung probe must not stall the build until the job timeout).
function Get-OutputLines([string] $Exe, [string[]] $Arguments) {
    try {
        $psi = [Diagnostics.ProcessStartInfo]::new($Exe)
        foreach ($a in $Arguments) { $psi.ArgumentList.Add($a) }
        $psi.UseShellExecute = $false
        $psi.RedirectStandardOutput = $true
        # ffmpeg and mpv print UTF-8 (e.g. mpv's "Copyright ©"), not the
        # console code page .NET would assume.
        $psi.StandardOutputEncoding = [Text.UTF8Encoding]::new($false)
        $p = [Diagnostics.Process]::Start($psi)
        $stdout = $p.StandardOutput.ReadToEndAsync()
        if (-not $p.WaitForExit(30000)) {
            $p.Kill($true)
            return $null
        }
        if ($p.ExitCode -ne 0 -or -not $stdout.Wait(5000)) { return $null }
        $lines = @($stdout.Result -split "`r?`n" | Where-Object { $_.Trim() } | ForEach-Object { $_.TrimEnd() })
        if ($lines.Count -eq 0) { return $null }
        return , $lines
    } catch {
        return $null
    }
}

# First line printed by `exe args`, or $null (see Get-OutputLines).
function Get-FirstLine([string] $Exe, [string[]] $Arguments) {
    $lines = Get-OutputLines $Exe $Arguments
    if ($lines) { return $lines[0] }
    return $null
}

# The first git commit hash that $Pattern (with the hash as group 1) finds in
# $Texts, or $null.
function Find-CommitHash([string[]] $Texts, [string] $Pattern) {
    foreach ($t in $Texts) {
        if ($t -and $t -match $Pattern) { return $Matches[1].ToLowerInvariant() }
    }
    return $null
}

# A release tag safe to put in a URL and a file name, or 'unknown'.
function Get-SafeTag([string] $Tag) {
    if ($Tag -match '^[A-Za-z0-9][A-Za-z0-9._-]*$' -and $Tag -ne 'unknown') { return $Tag }
    return 'unknown'
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
if (-not (Test-Path -LiteralPath (Join-Path $RepoRoot 'LICENSE') -PathType Leaf)) {
    Fail "LICENSE was not found in $RepoRoot; it becomes licenses\GloorpFlorp-LICENSE.txt in the package."
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
    # The notices call the mpv folder GPL software, so name any DLL that
    # comes with it. shinchiro's build ships Microsoft's d3dcompiler_43.dll.
    $mpvDllNote = @(foreach ($dll in $dlls) {
        if ($dll.Name -match '^d3dcompiler_\d+\.dll$') {
            "Also:     mpv\$($dll.Name) is Microsoft's Direct3D shader compiler,"
            '          which this mpv build includes for older Windows versions.'
            '          It is Microsoft software under Microsoft''s own terms,'
            '          not under the GPL.'
        } else {
            Write-Host "::warning::The mpv download contains $($dll.Name), which THIRD-PARTY-NOTICES.txt doesn't describe. Check its license and update packaging/package-windows.ps1."
            "Also:     mpv\$($dll.Name) came with the mpv build; the build scripts"
            '          linked below say where it comes from.'
        }
    }) -join "`n"
    $mpvLicenses = Copy-LicenseFile $mpvWork 'mpv'
    Write-Host "mpv: found $mpvLicenses license file(s) in the download"

    $mpvVersion = '(not checked: packaged on a non-Windows machine)'
    # mpv.exe has its own copy of FFmpeg built in, usually from a different
    # commit than ffmpeg.exe; only `mpv --version` tells which. For example:
    #   libplacebo version: v7.375.0 (v7.360.0-153-g1dd0226-dirty)
    #   FFmpeg version: N-127313-g8d8c75f2b
    $mpvFfmpegVersion = $null
    $mpvPlaceboVersion = $null
    if ($IsWindows) {
        # mpv.com is the console front end of mpv.exe; it prints to our pipe reliably.
        $mpvCom = Join-Path $mpvSource.DirectoryName 'mpv.com'
        $probe = if (Test-Path -LiteralPath $mpvCom) { $mpvCom } else { Join-Path $mpvDir 'mpv.exe' }
        $mpvLines = Get-OutputLines $probe @('--no-config', '--version')
        $mpvVersion = if ($mpvLines) { $mpvLines[0] } else { $null }
        foreach ($line in @($mpvLines)) {
            if (-not $mpvFfmpegVersion -and $line -match '^\s*FFmpeg version:\s*(\S.*)$') { $mpvFfmpegVersion = $Matches[1].Trim() }
            if (-not $mpvPlaceboVersion -and $line -match '^\s*libplacebo version:\s*(\S.*)$') { $mpvPlaceboVersion = $Matches[1].Trim() }
        }
        if (-not $mpvVersion) {
            Write-Host "::warning::Could not read the mpv version ('mpv --version' failed). The package is still built; test the viewer on a real PC."
            $mpvVersion = '(unknown: mpv --version failed on the build machine)'
        } elseif ($mpvVersion -match '^mpv v?(\d+)\.(\d+)' -and [int]$Matches[1] -eq 0 -and [int]$Matches[2] -lt 41) {
            # The viewer passes --hwdec-threads, which older mpv refuses (it wouldn't start).
            Fail "The downloaded mpv is too old ($mpvVersion). GloorpFlorp's viewer needs mpv 0.41 or newer; check the 'Download mpv' step."
        }
    }

    # License texts: GPL v3 (FFmpeg, and mpv as built here, which includes a
    # GPL v3 FFmpeg), GPL v2 (mpv's own code is GPL v2 or later) and
    # GloorpFlorp's own MIT license.
    Copy-Item -LiteralPath (Join-Path $PackagingDir 'licenses/GPL-3.0.txt') -Destination $LicensesDir
    Copy-Item -LiteralPath (Join-Path $PackagingDir 'licenses/GPL-2.0.txt') -Destination $LicensesDir
    Copy-Item -LiteralPath (Join-Path $RepoRoot 'LICENSE') -Destination (Join-Path $LicensesDir 'GloorpFlorp-LICENSE.txt')

    # --- Exact source of FFmpeg and mpv -----------------------------------
    # FFmpeg prints e.g. "ffmpeg version N-127271-gba987fe24d-20261010" (a
    # master build, commit ba987fe24d); BtbN's dated zips are named
    # ffmpeg-N-127271-gba987fe24d-win64-gpl.zip. mpv prints e.g.
    # "mpv v0.41.0-1122-gaccb7b282" and shinchiro's archive is named
    # mpv-x86_64-20261011-git-accb7b2821.7z (a longer hash of the same commit).
    # Anything that can't be worked out becomes 'unknown' plus a general link,
    # with a warning, instead of failing the build.

    $mpvAsset = Split-Path $MpvArchive -Leaf
    $ffCommit = Find-CommitHash @($ffVersion, (Split-Path $FfmpegZip -Leaf), (Split-Path $FfmpegUrl -Leaf)) '(?:^|[\s-])(?:N|n\d+(?:\.\d+)*)-\d+-g([0-9a-fA-F]{7,40})(?![0-9a-fA-F])'
    $mpvCommit = Find-CommitHash @($mpvVersion) '\bv?\d+(?:\.\d+)+-\d+-g([0-9a-fA-F]{7,40})(?![0-9a-fA-F])'
    $mpvAssetCommit = Find-CommitHash @($mpvAsset) '-git-([0-9a-fA-F]{7,40})(?![0-9a-fA-F])'
    if (-not $mpvCommit -or ($mpvAssetCommit -and $mpvAssetCommit.StartsWith($mpvCommit))) {
        $mpvCommit = $mpvAssetCommit
    }
    $mpvFfCommit = Find-CommitHash @($mpvFfmpegVersion) '^(?:N|n\d+(?:\.\d+)*)-\d+-g([0-9a-fA-F]{7,40})(?![0-9a-fA-F])'
    $FfmpegRelease = Get-SafeTag $FfmpegRelease
    $MpvRelease = Get-SafeTag $MpvRelease

    $ffSourceUrl = if ($ffCommit) { "https://github.com/FFmpeg/FFmpeg/tree/$ffCommit" } else { 'https://ffmpeg.org/download.html' }
    $ffBuildUrl = if ($FfmpegRelease -ne 'unknown') { "https://github.com/$FfmpegRepo/tree/$FfmpegRelease" } else { "https://github.com/$FfmpegRepo" }
    $mpvSourceUrl = if ($mpvCommit) { "https://github.com/mpv-player/mpv/tree/$mpvCommit" } else { 'https://github.com/mpv-player/mpv' }
    $mpvBuildUrl = if ($MpvRelease -ne 'unknown') { "https://github.com/$MpvRepo/tree/$MpvRelease" } else { "https://github.com/$MpvRepo" }
    $mpvFfSourceUrl = if ($mpvFfCommit) { "https://github.com/FFmpeg/FFmpeg/tree/$mpvFfCommit" } else { 'https://ffmpeg.org/download.html' }
    $ffReleaseText = if ($FfmpegRelease -ne 'unknown') { "release $FfmpegRelease of https://github.com/$FfmpegRepo" } else { 'unknown' }

    $unknown = @(
        if (-not $ffCommit) { 'the FFmpeg commit' }
        if (-not $mpvCommit) { 'the mpv commit' }
        if (-not $mpvFfCommit) { 'the commit of the FFmpeg inside mpv' }
        if ($FfmpegRelease -eq 'unknown') { "the $FfmpegRepo release" }
        if ($MpvRelease -eq 'unknown') { "the $MpvRepo release" }
    )
    if ($unknown) {
        Write-Host "::warning::Could not work out $($unknown -join ', ') of this build; VERSIONS.txt and THIRD-PARTY-NOTICES.txt link to the project in general instead."
    }
    Write-Host "Sources: FFmpeg $ffSourceUrl ($ffBuildUrl), mpv $mpvSourceUrl ($mpvBuildUrl), FFmpeg inside mpv $mpvFfSourceUrl"

    Write-WindowsText (Join-Path $LicensesDir 'SOURCES.txt') @(
        '# The exact source code of the FFmpeg and mpv builds in this package.'
        '# VERSIONS.txt and THIRD-PARTY-NOTICES.txt say the same in words.'
        '# (GloorpFlorp''s release workflow reads this file; keep the key=value lines.)'
        "ffmpeg_commit=$(if ($ffCommit) { $ffCommit } else { 'unknown' })"
        "ffmpeg_build_repo=$FfmpegRepo"
        "ffmpeg_build_release=$FfmpegRelease"
        "mpv_commit=$(if ($mpvCommit) { $mpvCommit } else { 'unknown' })"
        "mpv_build_repo=$MpvRepo"
        "mpv_build_release=$MpvRelease"
        "mpv_ffmpeg_commit=$(if ($mpvFfCommit) { $mpvFfCommit } else { 'unknown' })"
    )

    # --- Text files ------------------------------------------------------

    # GloorpFlorp's own version, from Cargo.toml.
    $appVersion = 'unknown'
    Push-Location $RepoRoot
    try {
        $metadata = & cargo metadata --no-deps --format-version 1 2>$null
        if ($LASTEXITCODE -eq 0) {
            $pkg = @(($metadata -join "`n" | ConvertFrom-Json).packages | Where-Object { $_.name -eq 'gloorpflorp' })
            if ($pkg.Count -eq 1) { $appVersion = $pkg[0].version }
        }
    } catch {
        $appVersion = 'unknown'
    } finally {
        Pop-Location
    }

    $source = if ($RepoUrl) { "$RepoUrl/commit/$Commit" } else { $Commit }
    Write-WindowsText (Join-Path $OutDir 'VERSIONS.txt') @(
        "GloorpFlorp $appVersion"
        "  Built from:  $source"
        "  Branch/tag:  $Ref"
        "  Built at:    $([DateTime]::UtcNow.ToString('yyyy-MM-dd HH:mm')) UTC"
        ''
        'FFmpeg (ffmpeg\ffmpeg.exe)'
        "  Version:     $ffVersion"
        "  Downloaded:  $FfmpegUrl"
        "  Build:       $ffReleaseText"
        "  Source code: $ffSourceUrl"
        "  Built with:  $ffBuildUrl"
        ''
        'mpv (mpv\mpv.exe)'
        "  Version:     $mpvVersion"
        "  Downloaded:  $mpvAsset from release $MpvRelease of https://github.com/$MpvRepo"
        "               https://github.com/$MpvRepo/releases/tag/$MpvRelease"
        "  Source code: $mpvSourceUrl"
        "  Built with:  $mpvBuildUrl"
        '  Built into mpv.exe (separate from ffmpeg\ffmpeg.exe):'
        "    FFmpeg:      $(if ($mpvFfmpegVersion) { $mpvFfmpegVersion } else { 'unknown' })"
        "    Source code: $mpvFfSourceUrl"
        "    libplacebo:  $(if ($mpvPlaceboVersion) { $mpvPlaceboVersion } else { 'unknown' })"
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

    # A tag build (v1.0.0, ...) becomes a GitHub Release with the source
    # archives attached (see the release job in build.yml).
    $releaseNote = if ($RepoUrl -and $RefType -eq 'tag' -and $Ref -match '^v\d') {
        @(
            'Copies of this exact FFmpeg and mpv source code, and of the build'
            'scripts that made these builds, are also attached to the GitHub'
            'release this zip comes from, so they stay available even if the'
            'links below stop working:'
            "  $RepoUrl/releases/tag/$Ref"
        ) -join "`n"
    } else {
        "This copy is not from a GitHub release; the links below lead to the`nsource code."
    }
    $notices = Get-Content -LiteralPath (Join-Path $PackagingDir 'third-party-notices.txt') -Raw
    if (-not $mpvDllNote) { $notices = $notices -replace '\{\{MPV_DLLS\}\}\r?\n', '' }
    $fill = [ordered]@{
        '{{RELEASE_SOURCES}}' = $releaseNote
        '{{FFMPEG_SOURCE}}'   = $ffSourceUrl
        '{{FFMPEG_BUILD}}'    = $ffBuildUrl
        '{{MPV_SOURCE}}'      = $mpvSourceUrl
        '{{MPV_BUILD}}'       = $mpvBuildUrl
        '{{MPV_FFMPEG_SOURCE}}' = $mpvFfSourceUrl
        '{{MPV_DLLS}}'        = $mpvDllNote
    }
    foreach ($key in $fill.Keys) { $notices = $notices.Replace($key, $fill[$key]) }
    if ($notices -match '\{\{[A-Z_]+\}\}') {
        Fail "Packaging bug: packaging/third-party-notices.txt has a placeholder this script doesn't fill in ($($Matches[0]))."
    }
    $notices = @($notices.TrimEnd() -split "`r?`n") + @('') + @($crates | ForEach-Object { "  $_" })
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
        'licenses/GPL-2.0.txt', 'licenses/GPL-3.0.txt', 'licenses/GloorpFlorp-LICENSE.txt', 'licenses/SOURCES.txt'
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
