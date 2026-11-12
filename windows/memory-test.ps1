# Mede a memória do telinha.exe do pacote, com um espectador conectado.
# Uso: windows/memory-test.ps1 -Package <pasta do pacote> [-Args '--test']
# Roda no CI (executor sem GPU, como uma VM): encoder e Direct3D por software.
[CmdletBinding()]
param([Parameter(Mandatory = $true)][string]$Package, [string[]]$Scenarios = @('--test', '--test --audio', '--monitor 0', '--monitor 0 --audio'))
$ErrorActionPreference = 'Stop'
$Package = (Resolve-Path $Package).Path
$env:PATH = "$Package;$Package\gstreamer\bin;$env:PATH"
$env:GST_PLUGIN_PATH_1_0 = "$Package\gstreamer\lib\gstreamer-1.0"
$env:GST_PLUGIN_SYSTEM_PATH_1_0 = $env:GST_PLUGIN_PATH_1_0
$env:GST_PLUGIN_SCANNER_1_0 = "$Package\gstreamer\libexec\gstreamer-1.0\gst-plugin-scanner.exe"
$env:GST_REGISTRY_1_0 = Join-Path $env:TEMP "telinha-mem-$PID.bin"

function Measure([Diagnostics.Process]$process, [string]$label) {
    $process.Refresh()
    '{0,-34} working set {1,5} MB · privada {2,5} MB · threads {3}' -f $label,
        [math]::Round($process.WorkingSet64 / 1MB), [math]::Round($process.PrivateMemorySize64 / 1MB), $process.Threads.Count
}

# Gera o registro do GStreamer antes, para não medir o scanner.
& "$Package\gstreamer\bin\gst-inspect-1.0.exe" coreelements *> $null

foreach ($scenario in $Scenarios) {
    "=== telinha $scenario"
    $log = Join-Path $env:TEMP "telinha-mem-$PID.log"
    $start = New-Object Diagnostics.ProcessStartInfo
    $start.FileName = "$Package\telinha.exe"
    $start.Arguments = "$scenario --bind 127.0.0.1 --port 18090"
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardInput = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    $server = [Diagnostics.Process]::Start($start)
    $output = $server.StandardOutput.ReadToEndAsync()
    $errors = $server.StandardError.ReadToEndAsync()
    $socket = New-Object Net.WebSockets.ClientWebSocket
    try {
        $ready = $false
        for ($i = 0; $i -lt 80; $i++) {
            if ($server.HasExited) { break }
            try {
                if ((Invoke-WebRequest -UseBasicParsing 'http://127.0.0.1:18090' -TimeoutSec 1).StatusCode -eq 200) { $ready = $true; break }
            } catch { Start-Sleep -Milliseconds 250 }
        }
        if (!$ready) { "  não iniciou (código $(if ($server.HasExited) { $server.ExitCode } else { '?' }))"; continue }
        Measure $server 'iniciado'
        Start-Sleep 12
        Measure $server 'ocioso (pipeline pausado)'
        $socket.ConnectAsync([uri]'ws://127.0.0.1:18090/ws', [Threading.CancellationToken]::None).GetAwaiter().GetResult()
        $hello = [Text.Encoding]::UTF8.GetBytes('{"type":"hello","name":"Memoria","device":"CI","client":"mem"}')
        $socket.SendAsync((New-Object 'ArraySegment[byte]' -ArgumentList @(,$hello)), 'Text', $true, [Threading.CancellationToken]::None).GetAwaiter().GetResult()
        Start-Sleep 5; Measure $server 'com espectador, 5 s'
        Start-Sleep 10; Measure $server 'com espectador, 15 s'
        Start-Sleep 15; Measure $server 'com espectador, 30 s'
        $socket.Dispose(); $socket = $null
        Start-Sleep 14
        Measure $server 'espectador saiu + pausa'
        $server.StandardInput.WriteLine('q')
        [void]$server.WaitForExit(8000)
    } finally {
        if ($socket) { $socket.Dispose() }
        if (!$server.HasExited) { $server.Kill(); $server.WaitForExit() }
        $text = $output.GetAwaiter().GetResult() + $errors.GetAwaiter().GetResult()
        ($text -split "`n" | Where-Object { $_ -match 'Encoder|Capturando|erro|Error|aviso' } | Select-Object -First 6) | ForEach-Object { "  | $_" }
        $server.Dispose()
    }
}
Remove-Item $env:GST_REGISTRY_1_0 -Force -ErrorAction SilentlyContinue
