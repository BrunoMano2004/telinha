[CmdletBinding()]
param([Parameter(Mandatory = $true)][string]$Package)
$ErrorActionPreference = 'Stop'
$Package = (Resolve-Path $Package).Path
$env:PATH = "$Package;$Package\gstreamer\bin;$env:PATH"
$env:GST_PLUGIN_PATH_1_0 = "$Package\gstreamer\lib\gstreamer-1.0"
$env:GST_PLUGIN_SYSTEM_PATH_1_0 = $env:GST_PLUGIN_PATH_1_0
$env:GST_PLUGIN_SCANNER_1_0 = "$Package\gstreamer\libexec\gstreamer-1.0\gst-plugin-scanner.exe"
$env:GST_REGISTRY_1_0 = Join-Path $env:TEMP "telinha-smoke-$PID.bin"
try {
    foreach ($plugin in @('d3d11screencapturesrc', 'wasapi2src', 'webrtcbin', 'nicesrc', 'dtlssrtpenc', 'opusenc', 'audiomixer', 'x264enc')) {
        & "$Package\gstreamer\bin\gst-inspect-1.0.exe" $plugin *> $null
        if ($LASTEXITCODE -ne 0) { throw "Plugin ausente: $plugin" }
    }
    & "$Package\telinha.exe" --help *> $null
    if ($LASTEXITCODE -ne 0) { throw 'O servidor nao inicia com as DLLs do pacote.' }
    & "$Package\cloudflared.exe" --version
    if ($LASTEXITCODE -ne 0) { throw 'cloudflared nao inicia.' }
    # Testa servidor, pipeline sintetico, parada via stdin e liberacao da porta.
    $start = New-Object Diagnostics.ProcessStartInfo
    $start.FileName = "$Package\telinha.exe"
    $start.Arguments = '--test --audio --encoder x264enc --bind 127.0.0.1 --port 18080'
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardInput = $true
    $server = [Diagnostics.Process]::Start($start)
    $socket = New-Object Net.WebSockets.ClientWebSocket
    $cancel = New-Object Threading.CancellationTokenSource
    $cancel.CancelAfter(15000)
    try {
        $ready = $false
        for ($i = 0; $i -lt 40; $i++) {
            if ($server.HasExited) { throw "Servidor encerrou antes de iniciar ($($server.ExitCode))." }
            try {
                $response = Invoke-WebRequest -UseBasicParsing 'http://127.0.0.1:18080' -TimeoutSec 1
                if ($response.StatusCode -eq 200) { $ready = $true; break }
            } catch { Start-Sleep -Milliseconds 250 }
        }
        if (!$ready) { throw 'O viewer nao respondeu.' }
        # Um espectador conectado exercita WebRTC e garante que Parar funciona
        # mesmo enquanto o websocket continua aberto.
        $socket.ConnectAsync([uri]'ws://127.0.0.1:18080/ws', $cancel.Token).GetAwaiter().GetResult()
        # A pagina se apresenta (nome, aparelho, id do navegador) antes de receber video.
        $hello = [Text.Encoding]::UTF8.GetBytes('{"type":"hello","name":"Smoke test","device":"CI","client":"smoke-test"}')
        $helloSegment = New-Object 'ArraySegment[byte]' -ArgumentList @(,$hello)
        $socket.SendAsync($helloSegment, [Net.WebSockets.WebSocketMessageType]::Text, $true, $cancel.Token).GetAwaiter().GetResult()
        $buffer = New-Object byte[] 65536
        $segment = New-Object 'ArraySegment[byte]' -ArgumentList @(,$buffer)
        $sdp = ''
        do {
            $received = $socket.ReceiveAsync($segment, $cancel.Token).GetAwaiter().GetResult()
            $sdp += [Text.Encoding]::UTF8.GetString($buffer, 0, $received.Count)
        } while (!$received.EndOfMessage)
        $offer = $sdp | ConvertFrom-Json
        if ($offer.type -ne 'offer' -or $offer.sdp -notmatch 'm=video' -or $offer.sdp -notmatch 'm=audio') {
            throw 'Oferta WebRTC sem trilhas de video e audio.'
        }
        $server.StandardInput.WriteLine('q')
        if (!$server.WaitForExit(5000)) { throw 'Servidor nao encerrou pelo comando q.' }
        if ($server.ExitCode -ne 0) { throw "Servidor falhou ($($server.ExitCode))." }
    } finally {
        $socket.Dispose()
        $cancel.Dispose()
        if (!$server.HasExited) { $server.Kill(); $server.WaitForExit() }
        $server.Dispose()
    }
} finally {
    Remove-Item $env:GST_REGISTRY_1_0 -Force -ErrorAction SilentlyContinue
}
