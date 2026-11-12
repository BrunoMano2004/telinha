pragma Singleton

import QtQuick
import Quickshell
import Quickshell.Io
import Caelestia
import Caelestia.I18n
import qs.utils

// Controla o processo do telinha (compartilhamento de tela via WebRTC) e
// expõe o estado lido da saída dele.
Singleton {
    id: root

    readonly property bool running: proc.running
    readonly property bool selecting: running && url === ""
    property string url: ""
    property int viewers: 0
    property string lastError: ""
    // Transmitir também o som do sistema. Salvo entre reinícios do shell;
    // vale a partir do próximo Start.
    property bool audio: true

    function setAudio(value: bool): void {
        audio = value;
        settings.setText(JSON.stringify({ audio: value }));
    }

    function start(): void {
        if (proc.running)
            return;
        url = "";
        viewers = 0;
        lastError = "";
        proc.running = true;
    }

    function stop(): void {
        proc.running = false;
    }

    function copyLink(): void {
        if (!url)
            return;
        Quickshell.clipboardText = url;
        Toaster.toast(Tr.tr("Link copied"), url, "content_copy");
    }

    Process {
        id: proc

        // --tunnel: link público pela Cloudflare, que abre para qualquer pessoa.
        // Usa o telinha do PATH (pacote do AUR) ou o de ~/.local/bin (./install.sh).
        // O exec mantém o PID, então parar o processo ainda manda o SIGTERM ao telinha.
        command: ["sh", "-c", 'exec "$(command -v telinha || echo "$HOME/.local/bin/telinha")" "$@"', "telinha", "--tunnel"].concat(root.audio ? ["--audio"] : [])

        stdout: SplitParser {
            onRead: line => {
                const link = line.match(/^Assista em: (\S+)/);
                if (link) {
                    root.url = link[1];
                    Quickshell.clipboardText = root.url;
                    Toaster.toast(Tr.tr("Screen share started"), Tr.tr("Link copied: %1").arg(root.url), "cast", Toast.Success);
                    return;
                }
                const total = line.match(/total (\d+)$/);
                if (total)
                    root.viewers = parseInt(total[1]);
            }
        }

        stderr: SplitParser {
            onRead: line => {
                if (line.trim() && !line.startsWith("aviso"))
                    root.lastError = line.replace(/^Error: /, "");
            }
        }

        onExited: code => { // qmllint disable signal-handler-parameters
            // Saída com erro antes do link = seletor cancelado ou falha ao iniciar.
            if (code !== 0 && root.url === "" && root.lastError)
                Toaster.toast(Tr.tr("Screen share failed"), root.lastError, "cast", Toast.Error);
            root.url = "";
            root.viewers = 0;
        }
    }

    FileView {
        id: settings

        printErrors: false
        path: `${Paths.state}/telinha.json`
        onLoaded: {
            try {
                root.audio = JSON.parse(text()).audio !== false;
            } catch (e) {}
        }
    }
}
