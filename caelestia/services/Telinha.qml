pragma Singleton

import QtQuick
import Quickshell
import Quickshell.Io
import Caelestia
import Caelestia.I18n
import qs.utils

// Controla o processo do telinha (compartilhamento de tela via WebRTC).
// Conversa com ele pelo stdin (comandos) e pela saída (eventos "@...").
Singleton {
    id: root

    readonly property bool running: proc.running
    readonly property bool selecting: running && url === ""
    property string url: ""
    property string lastError: ""
    // Vindos do telinha enquanto transmite.
    property var viewers: []   // [{id, name, device, ip, since}]
    property var blocked: []   // [{key, name, device, ip}]
    property var liveApps: []  // [{name, on, voice}] apps tocando som agora
    property bool switchingScreen: false
    // Jam: "" (transmissão normal), "jam" (você criou) ou "join" (entrou numa jam).
    property string mode: ""
    property string joinLink: ""
    property var jam: null     // {role, id, link, watch}
    property var room: null    // {locked, presenters: [...], viewers: [...]} (só o anfitrião recebe)
    property bool sharePaused: false
    readonly property bool inJam: mode !== ""
    readonly property bool jamHost: mode === "jam"
    // Telas na jam, sem contar a sua.
    readonly property var jamScreens: room ? room.presenters.filter(p => !p.host) : []
    // Log da transmissão atual (detalhes técnicos), para copiar e investigar.
    property var logLines: []

    function addLog(line: string): void {
        if (!line.trim())
            return;
        const lines = logLines.concat([line]);
        logLines = lines.length > 300 ? lines.slice(lines.length - 300) : lines;
    }

    function copyLog(): void {
        if (!logLines.length)
            return;
        Quickshell.clipboardText = logLines.join("\n");
        Toaster.toast(Tr.tr("Technical details copied"), Tr.tr("%1 lines").arg(logLines.length), "content_copy");
    }

    // Preferências (salvas em ~/.local/state/caelestia/telinha.json).
    property bool audio: true
    property int quality: 2
    // Qualidade automática: o telinha ajusta bitrate e fps pela rede de quem
    // assiste, pelo encoder e pelo uso de CPU. autoNow = o que ele escolheu agora.
    property bool autoQuality: true
    property var autoNow: null
    property var appChoices: ({}) // nome em minúsculas → entra no som?

    readonly property list<var> qualities: [
        { name: Tr.tr("Light"), detail: Tr.tr("3 Mbps · 30 fps · slow connections"), kbps: 3000, fps: 30 },
        { name: Tr.tr("Balanced"), detail: Tr.tr("6 Mbps · 60 fps"), kbps: 6000, fps: 60 },
        { name: Tr.tr("High"), detail: Tr.tr("12 Mbps · 60 fps · sharp text"), kbps: 12000, fps: 60 },
        { name: Tr.tr("Maximum"), detail: Tr.tr("20 Mbps · 60 fps · needs fast upload"), kbps: 20000, fps: 60 }
    ]
    readonly property var currentQuality: qualities[quality] ?? qualities[2]

    // Mesma lista do telinha (src/audio_rules.rs): fora por padrão, para não dar eco.
    readonly property list<string> voiceApps: ["discord", "vesktop", "webcord", "legcord", "armcord", "equibop", "webrtc", "teamspeak", "ts3client", "mumble", "zoom", "slack", "teams", "skype", "telegram", "signal", "whatsapp", "element", "jitsi"]

    function isVoiceApp(name: string): bool {
        const n = name.toLowerCase();
        return voiceApps.some(v => n.includes(v));
    }

    function appEnabled(name: string, voice: bool): bool {
        const choice = appChoices[name.toLowerCase()];
        return choice === undefined ? !voice : choice;
    }

    function start(): void {
        startMode("");
    }

    // Cria uma jam: o link mostra a sua tela e a de todo mundo que entrar.
    function startJam(): void {
        startMode("jam");
    }

    // Entra na jam do link copiado e compartilha a sua tela nela.
    function joinJam(): void {
        if (!proc.running)
            paste.running = true;
    }

    function openJam(): void {
        if (jam?.watch)
            Qt.openUrlExternally(jam.watch);
    }

    function setSharing(on: bool): void {
        send(`share ${on ? "on" : "off"}`);
    }

    function stopShare(id: int): void {
        send(`stop-share ${id}`);
    }

    function setLocked(on: bool): void {
        send(`lock ${on ? "on" : "off"}`);
    }

    function startMode(newMode: string): void {
        if (proc.running)
            return;
        mode = newMode;
        jam = null;
        room = null;
        sharePaused = false;
        url = "";
        lastError = "";
        viewers = [];
        logLines = [];
        blocked = [];
        liveApps = [];
        autoNow = null;
        switchingScreen = false;
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

    function send(command: string): void {
        if (proc.running)
            proc.write(command + "\n");
    }

    function kick(id: int): void {
        send(`kick ${id}`);
    }

    function block(id: int): void {
        send(`block ${id}`);
    }

    function unblock(key: string): void {
        send(`unblock ${key}`);
    }

    function switchScreen(): void {
        if (!running || switchingScreen)
            return;
        switchingScreen = true;
        send("screen");
    }

    function setQuality(index: int): void {
        quality = index;
        autoQuality = false;
        autoNow = null;
        save();
        send(`quality ${currentQuality.kbps} ${currentQuality.fps}`);
    }

    function setAutoQuality(): void {
        autoQuality = true;
        save();
        send("quality auto");
    }

    function setAudio(value: bool): void {
        audio = value;
        save();
    }

    function setApp(name: string, on: bool): void {
        const choices = Object.assign({}, appChoices);
        choices[name.toLowerCase()] = on;
        appChoices = choices;
        save();
        send(`app ${on ? "on" : "off"} ${name}`);
    }

    function save(): void {
        settings.setText(JSON.stringify({
            audio: audio,
            quality: quality,
            auto: autoQuality,
            apps: appChoices
        }));
    }

    function choiceArgs(on: bool): list<string> {
        // Nomes com vírgula quebrariam a lista do --audio-include/exclude.
        return Object.keys(appChoices).filter(n => appChoices[n] === on && !n.includes(",") && n.trim());
    }

    // Lê o link da jam da área de transferência (wl-paste) antes de entrar.
    Process {
        id: paste

        command: ["wl-paste", "--no-newline", "--type", "text"]
        stdout: StdioCollector {
            onStreamFinished: {
                const link = text.trim();
                if (!/^https?:\/\/\S+$/.test(link)) {
                    Toaster.toast(Tr.tr("No jam link copied"), Tr.tr("Copy the jam link someone sent you, then try again"), "content_paste", Toast.Error);
                    return;
                }
                root.joinLink = link;
                root.startMode("join");
            }
        }
    }

    Process {
        id: proc

        stdinEnabled: true
        // --tunnel: link público pela Cloudflare, que abre para qualquer pessoa.
        // Usa o telinha do PATH (pacote do AUR) ou o de ~/.local/bin (./install.sh).
        // O exec mantém o PID, então parar o processo ainda manda o SIGTERM ao telinha.
        command: {
            const args = ["sh", "-c", 'exec "$(command -v telinha || echo "$HOME/.local/bin/telinha")" "$@"', "telinha"];
            // Quem entra numa jam só abre uma conexão de saída: não precisa de túnel.
            if (root.mode === "join")
                args.push("--join", root.joinLink);
            else
                args.push("--tunnel");
            if (root.mode === "jam")
                args.push("--jam");
            if (root.autoQuality)
                args.push("--auto-quality");
            else
                args.push("--bitrate", String(root.currentQuality.kbps), "--fps", String(root.currentQuality.fps));
            if (root.audio) {
                args.push("--audio");
                const include = root.choiceArgs(true);
                const exclude = root.choiceArgs(false);
                if (include.length)
                    args.push("--audio-include", include.join(","));
                if (exclude.length)
                    args.push("--audio-exclude", exclude.join(","));
            }
            return args;
        }

        stdout: SplitParser {
            onRead: line => {
                const screen = line.match(/^@screen (\w+)/);
                if (screen) {
                    if (screen[1] !== "choosing")
                        root.switchingScreen = false;
                    return;
                }
                const share = line.match(/^@share (on|off)/);
                if (share) {
                    root.sharePaused = share[1] === "off";
                    return;
                }
                const event = line.match(/^@([\w-]+) (.*)$/);
                if (event) {
                    let data;
                    try {
                        data = JSON.parse(event[2]);
                    } catch (e) {
                        return;
                    }
                    if (event[1] === "viewers")
                        root.viewers = data;
                    else if (event[1] === "blocked")
                        root.blocked = data;
                    else if (event[1] === "apps")
                        root.liveApps = data;
                    else if (event[1] === "quality")
                        root.autoNow = data.auto ? data : null;
                    else if (event[1] === "room")
                        root.room = data;
                    else if (event[1] === "jam") {
                        // Jam no ar: o link (para os outros) vai para a área de
                        // transferência e a sua página (sem a sua tela) abre.
                        root.jam = data;
                        root.url = data.link;
                        Quickshell.clipboardText = data.link;
                        if (data.role === "host")
                            Toaster.toast(Tr.tr("Jam started"), Tr.tr("Link copied: %1").arg(data.link), "groups", Toast.Success);
                        else
                            Toaster.toast(Tr.tr("You joined the jam"), Tr.tr("Your screen is shared; opening the other screens"), "groups", Toast.Success);
                        Qt.openUrlExternally(data.watch);
                    } else if (event[1] === "jam-ended")
                        Toaster.toast(Tr.tr("Jam ended"), data.reason, "groups", Toast.Info);
                    return;
                }
                root.addLog(line);
                const link = line.match(/^Assista em: (\S+)/);
                if (link) {
                    root.url = link[1];
                    Quickshell.clipboardText = root.url;
                    Toaster.toast(Tr.tr("Screen share started"), Tr.tr("Link copied: %1").arg(root.url), "cast", Toast.Success);
                }
            }
        }

        stderr: SplitParser {
            onRead: line => {
                root.addLog(line);
                if (line.trim() && !line.startsWith("aviso"))
                    root.lastError = line.replace(/^Error: /, "");
            }
        }

        onExited: code => { // qmllint disable signal-handler-parameters
            // Saída com erro antes do link = seletor cancelado ou falha ao iniciar.
            if (code !== 0 && root.url === "" && root.lastError)
                Toaster.toast(Tr.tr("Screen share failed"), root.lastError, "cast", Toast.Error);
            root.url = "";
            root.viewers = [];
            root.liveApps = [];
            root.autoNow = null;
            root.switchingScreen = false;
            root.jam = null;
            root.room = null;
            root.sharePaused = false;
        }
    }

    FileView {
        id: settings

        printErrors: false
        path: `${Paths.state}/telinha.json`
        onLoaded: {
            try {
                const data = JSON.parse(text());
                root.audio = data.audio !== false;
                if (Number.isInteger(data.quality) && data.quality >= 0 && data.quality < root.qualities.length)
                    root.quality = data.quality;
                root.autoQuality = data.auto !== false;
                if (data.apps && typeof data.apps === "object")
                    root.appChoices = data.apps;
            } catch (e) {}
        }
    }
}
