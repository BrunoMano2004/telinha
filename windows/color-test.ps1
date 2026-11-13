# Confere se as cores sobrevivem ao caminho do Windows (conversão -> encoder ->
# decodificador): preto deve sair (0,0,0), branco (255,255,255), vermelho (255,0,0).
# Preto saindo ~16 = faixa limitada anunciada como completa (imagem "lavada").
# Uso: windows/color-test.ps1 (baixa o runtime do GStreamer usado no pacote).
$ErrorActionPreference = 'Stop'
$work = Join-Path $env:TEMP 'telinha-color'
New-Item -ItemType Directory -Force $work | Out-Null
$version = '1.26.11'
$msi = Join-Path $work "gstreamer-1.0-msvc-x86_64-$version.msi"
if (!(Test-Path $msi)) {
    Invoke-WebRequest -UseBasicParsing "https://gstreamer.freedesktop.org/data/pkg/windows/$version/msvc/gstreamer-1.0-msvc-x86_64-$version.msi" -OutFile $msi
}
if ((Get-FileHash $msi -Algorithm SHA256).Hash -ne '31cbc21fa0950b5c1e79c80959b2799805cb05a7a35953a13a9f790776137605') { throw 'Checksum incorreto' }
$process = Start-Process msiexec.exe -Wait -PassThru -ArgumentList @('/a', "`"$msi`"", '/qn', "TARGETDIR=`"$work\sdk`"")
if ($process.ExitCode -notin @(0, 3010)) { throw "Falha ao extrair ($($process.ExitCode))" }
$launch = (Get-ChildItem "$work\sdk" -Recurse -Filter gst-launch-1.0.exe | Select-Object -First 1).FullName
$env:PATH = "$(Split-Path $launch);$env:PATH"
Add-Type -AssemblyName System.Drawing

$encoders = @('nvd3d11h264enc', 'qsvh264enc', 'amfh264enc', 'mfh264enc', 'x264enc') | Where-Object {
    & gst-inspect-1.0.exe $_ *> $null; $LASTEXITCODE -eq 0
}
"Encoders disponíveis: $($encoders -join ', ')"
# Os mesmos caminhos do app (src/capture.rs, windows_convert): a captura chega
# em memória D3D11; encoders sem entrada D3D11 recebem o quadro baixado da GPU.
$paths = [ordered]@{
    'D3D11 -> encoder' = 'd3d11upload ! d3d11convert ! video/x-raw(memory:D3D11Memory),format=NV12'
    'D3D11 -> download -> CPU' = 'd3d11upload ! d3d11download ! videoconvert ! video/x-raw,format=NV12'
}
foreach ($encoder in $encoders) {
    foreach ($path in $paths.GetEnumerator()) {
        "=== $encoder · $($path.Key)"
        foreach ($pattern in 'black', 'white', 'red') {
            $out = Join-Path $work "$encoder-$pattern-%d.png"
            Remove-Item (Join-Path $work "$encoder-$pattern-*.png") -ErrorAction SilentlyContinue
            $log = & gst-launch-1.0.exe -v videotestsrc num-buffers=10 pattern=$pattern `
                '!' 'video/x-raw,format=BGRA,width=640,height=360,framerate=30/1' '!' ($path.Value -split ' ') `
                '!' $encoder '!' h264parse '!' avdec_h264 '!' videoconvert '!' 'video/x-raw,format=RGB' `
                '!' pngenc '!' multifilesink "location=$out" 2>&1
            $last = Get-ChildItem (Join-Path $work "$encoder-$pattern-*.png") -ErrorAction SilentlyContinue | Sort-Object LastWriteTime | Select-Object -Last 1
            if (!$last) { "  $pattern : falhou ($((($log | Select-String 'ERROR|erro' | Select-Object -First 1)) -replace '\s+', ' '))"; break }
            $bitmap = [Drawing.Bitmap]::FromFile($last.FullName)
            $pixel = $bitmap.GetPixel(320, 180); $bitmap.Dispose()
            $caps = ($log | Select-String 'h264parse0.GstPad:src: caps' | Select-Object -Last 1) -replace '.*colorimetry=\(string\)([^,]+).*', '$1'
            '  {0,-6} -> ({1,3},{2,3},{3,3})   colorimetria anunciada: {4}' -f $pattern, $pixel.R, $pixel.G, $pixel.B, $caps
        }
    }
}
