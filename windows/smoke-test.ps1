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

    # Jam: anfitrião + um convidado (--join) e um navegador simulado. A
    # miniatura vem só com vídeo; a tela selecionada (camada alta), com som.
    # Ao encerrar o anfitrião, o convidado sai sozinho ("a jam terminou").
    function Start-Telinha([string]$arguments) {
        $info = New-Object Diagnostics.ProcessStartInfo
        $info.FileName = "$Package\telinha.exe"
        $info.Arguments = $arguments
        $info.UseShellExecute = $false
        $info.CreateNoWindow = $true
        $info.RedirectStandardInput = $true
        return [Diagnostics.Process]::Start($info)
    }
    function Receive-Json($ws, $token) {
        $buffer = New-Object byte[] 65536
        $segment = New-Object 'ArraySegment[byte]' -ArgumentList @(,$buffer)
        $text = ''
        do {
            $received = $ws.ReceiveAsync($segment, $token).GetAwaiter().GetResult()
            $text += [Text.Encoding]::UTF8.GetString($buffer, 0, $received.Count)
        } while (!$received.EndOfMessage)
        if ($received.MessageType -ne [Net.WebSockets.WebSocketMessageType]::Text) { return $null }
        return $text | ConvertFrom-Json
    }
    function Send-Json($ws, $token, [string]$json) {
        $bytes = [Text.Encoding]::UTF8.GetBytes($json)
        $segment = New-Object 'ArraySegment[byte]' -ArgumentList @(,$bytes)
        $ws.SendAsync($segment, [Net.WebSockets.WebSocketMessageType]::Text, $true, $token).GetAwaiter().GetResult()
    }
    $jamHost = Start-Telinha '--jam --test --audio --encoder x264enc --bind 127.0.0.1 --port 18081 --name Anfitriao'
    $guest = $null
    $socket = New-Object Net.WebSockets.ClientWebSocket
    $cancel = New-Object Threading.CancellationTokenSource
    $cancel.CancelAfter(30000)
    try {
        $ready = $false
        for ($i = 0; $i -lt 40; $i++) {
            if ($jamHost.HasExited) { throw "Anfitriao da jam encerrou antes de iniciar ($($jamHost.ExitCode))." }
            try {
                $response = Invoke-WebRequest -UseBasicParsing 'http://127.0.0.1:18081' -TimeoutSec 1
                if ($response.StatusCode -eq 200) { $ready = $true; break }
            } catch { Start-Sleep -Milliseconds 250 }
        }
        if (!$ready) { throw 'A pagina da jam nao respondeu.' }
        $guest = Start-Telinha '--join http://127.0.0.1:18081 --test --audio --encoder x264enc --name Convidado'
        $socket.ConnectAsync([uri]'ws://127.0.0.1:18081/ws', $cancel.Token).GetAwaiter().GetResult()
        Send-Json $socket $cancel.Token '{"type":"hello","name":"Smoke test","device":"CI","client":"smoke-jam"}'
        # Espera a sala ter as duas telas.
        $room = $null
        while ($true) {
            $msg = Receive-Json $socket $cancel.Token
            if ($msg -and $msg.type -eq 'room' -and @($msg.presenters).Count -ge 2) { $room = $msg; break }
        }
        $hostId = (@($room.presenters) | Where-Object { $_.host }).id
        $guestId = (@($room.presenters) | Where-Object { -not $_.host }).id
        Send-Json $socket $cancel.Token "{""type"":""watch"",""sid"":1,""pub"":$guestId,""layer"":""low"",""mode"":""webrtc""}"
        Send-Json $socket $cancel.Token "{""type"":""watch"",""sid"":2,""pub"":$hostId,""layer"":""high"",""mode"":""webrtc""}"
        $offers = @{}
        while ($offers.Count -lt 2) {
            $msg = Receive-Json $socket $cancel.Token
            if ($msg -and $msg.type -eq 'closed') { throw "Sessao $($msg.sid) recusada: $($msg.reason)" }
            if ($msg -and $msg.type -eq 'signal' -and $msg.payload.type -eq 'offer') { $offers[[int]$msg.sid] = $msg.payload.sdp }
        }
        if ($offers[1] -notmatch 'm=video' -or $offers[1] -match 'm=audio') { throw 'Miniatura da jam deveria ter so video.' }
        if ($offers[2] -notmatch 'm=video' -or $offers[2] -notmatch 'm=audio') { throw 'Tela selecionada da jam deveria ter video e som.' }
        $jamHost.StandardInput.WriteLine('q')
        if (!$jamHost.WaitForExit(5000)) { throw 'Anfitriao da jam nao encerrou pelo comando q.' }
        if ($jamHost.ExitCode -ne 0) { throw "Anfitriao da jam falhou ($($jamHost.ExitCode))." }
        if (!$guest.WaitForExit(8000)) { throw 'O convidado nao saiu quando a jam terminou.' }
        if ($guest.ExitCode -ne 0) { throw "Convidado da jam falhou ($($guest.ExitCode))." }
    } finally {
        $socket.Dispose()
        $cancel.Dispose()
        foreach ($p in @($jamHost, $guest)) {
            if ($p) {
                if (!$p.HasExited) { $p.Kill(); $p.WaitForExit() }
                $p.Dispose()
            }
        }
    }

    # Jam ligada durante a transmissão (`jam on`), com um convidado que entrou
    # pelo botão da página (--viewer-id). Ao desligar (`jam off`), o convidado
    # sai sozinho e a página recebe "reload" (volta a ser a transmissão normal).
    $soloHost = Start-Telinha '--test --encoder x264enc --bind 127.0.0.1 --port 18082 --name Anfitriao'
    $guest = $null
    $socket = New-Object Net.WebSockets.ClientWebSocket
    $cancel = New-Object Threading.CancellationTokenSource
    $cancel.CancelAfter(30000)
    try {
        $ready = $false
        for ($i = 0; $i -lt 40; $i++) {
            if ($soloHost.HasExited) { throw "Transmissao encerrou antes de iniciar ($($soloHost.ExitCode))." }
            try {
                $response = Invoke-WebRequest -UseBasicParsing 'http://127.0.0.1:18082' -TimeoutSec 1
                if ($response.StatusCode -eq 200) { $ready = $true; break }
            } catch { Start-Sleep -Milliseconds 250 }
        }
        if (!$ready) { throw 'A transmissao nao respondeu.' }
        if ($response.Content -match 'Compartilhar minha tela') { throw 'Sem jam, o link deveria servir a pagina normal.' }
        $soloHost.StandardInput.WriteLine('jam on')
        Start-Sleep -Milliseconds 500
        $response = Invoke-WebRequest -UseBasicParsing 'http://127.0.0.1:18082' -TimeoutSec 5
        if ($response.Content -notmatch 'Compartilhar minha tela') { throw 'Com a jam ligada, o link deveria servir a pagina da jam.' }
        $guest = Start-Telinha '--join http://127.0.0.1:18082 --test --encoder x264enc --name Convidado --viewer-id smoke-page'
        $socket.ConnectAsync([uri]'ws://127.0.0.1:18082/ws', $cancel.Token).GetAwaiter().GetResult()
        Send-Json $socket $cancel.Token '{"type":"hello","name":"Smoke test","device":"CI","client":"smoke-toggle","page":"jam"}'
        while ($true) {
            $msg = Receive-Json $socket $cancel.Token
            if ($msg -and $msg.type -eq 'room' -and @($msg.presenters).Count -ge 2) { break }
        }
        if (-not (@($msg.presenters) | Where-Object { $_.client -eq 'smoke-page' })) { throw 'O convidado deveria usar o id da pagina que o chamou.' }
        $soloHost.StandardInput.WriteLine('jam off')
        while ($true) {
            $msg = Receive-Json $socket $cancel.Token
            if ($msg -and $msg.type -eq 'reload') { break }
        }
        if (!$guest.WaitForExit(8000)) { throw 'O convidado nao saiu quando a jam foi desligada.' }
        if ($guest.ExitCode -ne 0) { throw "Convidado falhou ao sair da jam desligada ($($guest.ExitCode))." }
        $soloHost.StandardInput.WriteLine('q')
        if (!$soloHost.WaitForExit(5000)) { throw 'A transmissao nao encerrou pelo comando q.' }
        if ($soloHost.ExitCode -ne 0) { throw "A transmissao falhou ($($soloHost.ExitCode))." }
    } finally {
        $socket.Dispose()
        $cancel.Dispose()
        foreach ($p in @($soloHost, $guest)) {
            if ($p) {
                if (!$p.HasExited) { $p.Kill(); $p.WaitForExit() }
                $p.Dispose()
            }
        }
    }
} finally {
    Remove-Item $env:GST_REGISTRY_1_0 -Force -ErrorAction SilentlyContinue
}
