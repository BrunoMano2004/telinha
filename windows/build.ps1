# Gera um instalador offline. Apenas a maquina de build precisa de Rust e C++.
[CmdletBinding()]
param(
    [string]$GStreamerRoot,
    [string]$Iscc,
    [switch]$SkipBuild
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$repo = Split-Path $PSScriptRoot -Parent
$work = Join-Path $repo 'target\windows-deps'
$payload = Join-Path $repo 'target\windows-package'
$dist = Join-Path $repo 'dist'
New-Item -ItemType Directory -Force $work, $payload, $dist | Out-Null
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

function Get-VerifiedFile([string]$Url, [string]$Path, [string]$Hash) {
    if (!(Test-Path $Path) -or (Get-FileHash $Path -Algorithm SHA256).Hash -ne $Hash) {
        Invoke-WebRequest -UseBasicParsing $Url -OutFile $Path
    }
    if ((Get-FileHash $Path -Algorithm SHA256).Hash -ne $Hash) { throw "Checksum incorreto: $Path" }
}
function Run-Checked([string]$Program, [string[]]$Arguments) {
    & $Program @Arguments
    if ($LASTEXITCODE -ne 0) { throw "$Program terminou com codigo $LASTEXITCODE" }
}

if (!$GStreamerRoot) {
    # 1.26.11: evita a regressao de process loopback do GStreamer 1.28.0-1.28.8.
    $version = '1.26.11'
    $base = "https://gstreamer.freedesktop.org/data/pkg/windows/$version/msvc"
    $packages = @(
        @{ Name = "gstreamer-1.0-msvc-x86_64-$version.msi"; Hash = '31cbc21fa0950b5c1e79c80959b2799805cb05a7a35953a13a9f790776137605' },
        @{ Name = "gstreamer-1.0-devel-msvc-x86_64-$version.msi"; Hash = 'af0a0720692052aaf5fb0cf2847ef34a87016ef0f914328d516f049e6c86f587' }
    )
    foreach ($package in $packages) {
        $msi = Join-Path $work $package.Name
        Get-VerifiedFile "$base/$($package.Name)" $msi $package.Hash
        $process = Start-Process msiexec.exe -Wait -PassThru -ArgumentList @('/a', "`"$msi`"", '/qn', "TARGETDIR=`"$work\sdk`"")
        if ($process.ExitCode -notin @(0, 3010)) { throw "Falha ao extrair $msi ($($process.ExitCode))" }
    }
    $launch = Get-ChildItem "$work\sdk" -Recurse -Filter gst-launch-1.0.exe | Select-Object -First 1
    if (!$launch) { throw 'GStreamer nao encontrado depois da extracao.' }
    $GStreamerRoot = Split-Path $launch.DirectoryName -Parent
}
$GStreamerRoot = (Resolve-Path $GStreamerRoot).Path
$env:GSTREAMER_1_0_ROOT_MSVC_X86_64 = $GStreamerRoot
$env:PKG_CONFIG_PATH = "$GStreamerRoot\lib\pkgconfig"
$env:PATH = "$GStreamerRoot\bin;$env:PATH"
# Os arquivos .pc do SDK sao relocaveis via pkg-config --define-prefix.
if (!(Get-Command pkg-config.exe -ErrorAction SilentlyContinue)) { throw 'pkg-config.exe ausente no SDK do GStreamer.' }
Push-Location $repo
try {
    if (!$SkipBuild) {
        Run-Checked 'cargo' @('test', '--locked', '--target', 'x86_64-pc-windows-msvc')
        Run-Checked 'cargo' @('build', '--locked', '--release', '--target', 'x86_64-pc-windows-msvc')
    }
    Copy-Item 'target\x86_64-pc-windows-msvc\release\telinha.exe' "$payload\telinha.exe" -Force
    Run-Checked "$PSScriptRoot\build-ui.cmd" @("$payload\Telinha Desktop.exe")
    Remove-Item "$payload\Telinha Desktop.exe.obj" -Force -ErrorAction SilentlyContinue
    Remove-Item "$payload\Telinha Desktop.exe.res" -Force -ErrorAction SilentlyContinue
    # Copie apenas runtime, plugins, scanner e licencas, sem SDK ou compiladores.
    foreach ($directory in @('bin', 'lib\gstreamer-1.0', 'libexec\gstreamer-1.0')) {
        $source = Join-Path $GStreamerRoot $directory
        if (!(Test-Path $source)) { throw "Diretorio do runtime ausente: $source" }
        $destination = Join-Path "$payload\gstreamer" $directory
        New-Item -ItemType Directory -Force $destination | Out-Null
        Copy-Item "$source\*" $destination -Recurse -Force
    }
    Get-ChildItem "$payload\gstreamer" -Recurse -Include *.pdb,*.lib,*.a,*.h | Remove-Item -Force
    Get-VerifiedFile 'https://github.com/cloudflare/cloudflared/releases/download/2026.10.7/cloudflared-windows-amd64.exe' "$payload\cloudflared.exe" '2493dbc168923bb3e59e5e7716f71deced92992eea9acd6e9d74f84ac7751343'
    New-Item -ItemType Directory -Force "$payload\licenses" | Out-Null
    Invoke-WebRequest -UseBasicParsing 'https://raw.githubusercontent.com/GStreamer/gstreamer/1.26.11/subprojects/gstreamer/COPYING' -OutFile "$payload\licenses\GStreamer-COPYING.txt"
    if (Test-Path "$GStreamerRoot\share\licenses") {
        Copy-Item "$GStreamerRoot\share\licenses" "$payload\licenses\gstreamer" -Recurse -Force
    }
    Invoke-WebRequest -UseBasicParsing 'https://raw.githubusercontent.com/cloudflare/cloudflared/2026.10.7/LICENSE' -OutFile "$payload\licenses\cloudflared.txt"
    Copy-Item "$PSScriptRoot\THIRD-PARTY.md" "$payload\licenses\THIRD-PARTY.md" -Force
    # CRT local ao aplicativo: nao pede administrador nem instala runtime global.
    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    $vs = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    $redist = Get-ChildItem "$vs\VC\Redist\MSVC\*\x64\Microsoft.VC*.CRT" -Directory | Sort-Object FullName -Descending | Select-Object -First 1
    if (!$redist) { throw 'Runtime redistribuivel do Visual C++ ausente.' }
    Copy-Item "$($redist.FullName)\*.dll" $payload -Force
    Copy-Item "$($redist.FullName)\*.dll" "$payload\gstreamer\bin" -Force
    Copy-Item "$($redist.FullName)\*.dll" "$payload\gstreamer\libexec\gstreamer-1.0" -Force
    & "$PSScriptRoot\smoke-test.ps1" -Package $payload
    if (!$Iscc) {
        $Iscc = "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe"
        if (!(Test-Path $Iscc)) { $Iscc = (Get-Command ISCC.exe -ErrorAction Stop).Source }
    }
    $appVersion = (Select-String -Path Cargo.toml -Pattern '^version = "([^"]+)"').Matches[0].Groups[1].Value
    Run-Checked $Iscc @("/DPayloadDir=$payload", "/DOutputDir=$dist", "/DAppVersion=$appVersion", "$PSScriptRoot\installer.iss")
    Get-ChildItem "$dist\*.exe" | ForEach-Object {
        $hash = (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
        "$hash  $($_.Name)" | Set-Content "$($_.FullName).sha256" -Encoding ascii
    }
} finally { Pop-Location }
Write-Host "Instalador pronto em $dist"
