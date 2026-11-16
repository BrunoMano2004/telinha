pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Layouts
import Quickshell
import Caelestia.Config
import Caelestia.I18n
import qs.components
import qs.components.controls
import qs.services
import qs.modules.nexus.common

// Configurações do compartilhamento de tela (telinha): transmissão,
// espectadores, qualidade e som. Mudanças valem ao vivo quando possível.
PageBase {
    id: root

    // Relógio para o "há X min" de cada espectador.
    property real nowSec: Date.now() / 1000

    // Primeiro item: automática; os outros seguem Telinha.qualities (índice - 1).
    readonly property list<MenuItem> qualityItems: [
        MenuItem {
            text: Tr.tr("Automatic")
        },
        MenuItem {
            text: Telinha.qualities[0].name
        },
        MenuItem {
            text: Telinha.qualities[1].name
        },
        MenuItem {
            text: Telinha.qualities[2].name
        },
        MenuItem {
            text: Telinha.qualities[3].name
        }
    ]

    // Apps que tocam ou tocaram som: os de agora (do telinha ou do PipeWire)
    // e os que já têm uma escolha salva.
    readonly property var appList: {
        const map = new Map();
        const add = (name, voice) => {
            if (name && !map.has(name.toLowerCase()))
                map.set(name.toLowerCase(), {
                    name: name,
                    voice: voice
                });
        };
        for (const a of Telinha.liveApps)
            add(a.name, a.voice);
        for (const s of Audio.streams)
            add(s?.properties?.["application.name"] || s?.name || "", Telinha.isVoiceApp(s?.properties?.["application.name"] || s?.name || ""));
        for (const k of Object.keys(Telinha.appChoices))
            add(k, Telinha.isVoiceApp(k));
        return [...map.values()].sort((a, b) => a.name.localeCompare(b.name));
    }

    function since(sec: real): string {
        const min = Math.max(0, Math.floor((nowSec - sec) / 60));
        if (min < 1)
            return Tr.tr("just joined");
        if (min < 60)
            return Tr.tr("%1 min").arg(min);
        return Tr.tr("%1 h %2 min").arg(Math.floor(min / 60)).arg(min % 60);
    }

    title: Tr.tr("Screen share")

    // Propriedade, não filho: o PageBase só aceita itens visuais como filhos.
    readonly property Timer clock: Timer {
        running: Telinha.viewers.length > 0
        repeat: true
        interval: 30000
        triggeredOnStart: true
        onTriggered: root.nowSec = Date.now() / 1000
    }

    ColumnLayout {
        anchors.horizontalCenter: parent.horizontalCenter
        anchors.top: parent.top
        width: root.cappedWidth
        spacing: Tokens.spacing.extraSmall / 2

        // Transmissão
        SectionHeader {
            first: true
            text: Tr.tr("Sharing")
        }

        RowButton {
            first: true
            icon: Telinha.running ? "stop_circle" : "cast"
            text: {
                if (!Telinha.running)
                    return Tr.tr("Start sharing");
                if (Telinha.jamHost)
                    return Tr.tr("End the jam");
                return Telinha.inJam ? Tr.tr("Leave the jam") : Tr.tr("Stop sharing");
            }
            subtext: {
                if (Telinha.selecting)
                    return Tr.tr("Choose a screen in the system picker...");
                if (Telinha.jamHost)
                    return Tr.tr("Everyone in the jam sees that it ended");
                if (Telinha.running)
                    return Tr.tr("Live · public link, anyone with it can watch");
                return Tr.tr("Creates a public link and copies it");
            }
            onClicked: Telinha.running ? Telinha.stop() : Telinha.start()
        }

        RowButton {
            disabled: !Telinha.url
            icon: "link"
            text: Tr.tr("Copy link")
            subtext: Telinha.url || Tr.tr("Available once sharing starts")
            onClicked: Telinha.copyLink()
        }

        RowButton {
            last: true
            disabled: !Telinha.url || Telinha.switchingScreen
            icon: "screen_share"
            text: Telinha.switchingScreen ? Tr.tr("Choosing a new screen...") : Tr.tr("Switch screen")
            subtext: Tr.tr("Pick another screen or window without disconnecting viewers")
            onClicked: Telinha.switchScreen()
        }

        // Jam: várias telas num link só
        SectionHeader {
            text: Tr.tr("Jam")
        }

        RowButton {
            first: true
            visible: !Telinha.running
            icon: "groups"
            text: Tr.tr("Create a jam")
            subtext: Tr.tr("Everyone who joins with telinha shares their screen; one link shows all screens")
            onClicked: Telinha.startJam()
        }

        RowButton {
            last: true
            visible: !Telinha.running
            icon: "login"
            text: Tr.tr("Join the copied jam")
            subtext: Tr.tr("Copy the jam link someone sent you, then click here to share your screen in it")
            onClicked: Telinha.joinJam()
        }

        // A transmissão vira jam (e volta) sem parar: quem assiste ganha o
        // botão "Compartilhar minha tela" na página.
        ToggleRow {
            visible: Telinha.running && Telinha.mode !== "join"
            first: true
            last: !Telinha.jamHost
            text: Tr.tr("Let viewers share their screens")
            subtext: Telinha.jamHost ? Tr.tr("Anyone watching who has telinha can share too; turning off removes their screens") : Tr.tr("Turns this share into a jam: the page gets a \"Share my screen\" button")
            checked: Telinha.jamHost
            onToggled: Telinha.setJam(checked)
        }

        RowButton {
            first: Telinha.mode === "join"
            visible: Telinha.inJam && !!Telinha.jam?.watch
            disabled: !Telinha.jam
            icon: "open_in_new"
            text: Tr.tr("Open the jam")
            subtext: Tr.tr("Everyone else's screens. Click one to see it big and hear it; yours stays out")
            onClicked: Telinha.openJam()
        }

        RowButton {
            first: Telinha.mode === "join" && !Telinha.jam?.watch
            last: !Telinha.jamHost
            visible: Telinha.inJam
            disabled: !Telinha.jam
            icon: Telinha.sharePaused ? "play_circle" : "pause_circle"
            text: Telinha.sharePaused ? Tr.tr("Resume my screen") : Tr.tr("Pause my screen")
            subtext: Telinha.sharePaused ? Tr.tr("Your screen is hidden from the jam") : Tr.tr("Hides your screen from the jam without leaving")
            onClicked: Telinha.setSharing(Telinha.sharePaused)
        }

        ToggleRow {
            visible: Telinha.jamHost
            text: Tr.tr("Ask before someone shares")
            subtext: Tr.tr("Each new screen waits for you to accept it here")
            checked: Telinha.room?.approve ?? Telinha.approve
            onToggled: Telinha.setApprove(checked)
        }

        // Pedidos para entrar (com "pedir antes" ligado).
        Repeater {
            model: Telinha.jamHost ? Telinha.jamRequests : []

            PersonRow {
                id: requestRow

                required property var modelData

                icon: "person_add"
                name: modelData.name
                detail: [Tr.tr("wants to share their screen"), modelData.os === "windows" ? "Windows" : "Linux", modelData.ip].filter(s => s).join(" · ")

                IconButton {
                    icon: "check"
                    type: IconButton.Text
                    onClicked: Telinha.acceptRequest(requestRow.modelData.id)
                }

                IconButton {
                    icon: "close"
                    type: IconButton.Text
                    onClicked: Telinha.denyRequest(requestRow.modelData.id)
                }
            }
        }

        ToggleRow {
            visible: Telinha.jamHost
            last: Telinha.jamScreens.length === 0
            text: Tr.tr("Lock the jam")
            subtext: Tr.tr("Nobody new gets in; who is already in stays")
            checked: Telinha.room?.locked ?? false
            onToggled: Telinha.setLocked(checked)
        }

        Repeater {
            model: Telinha.jamHost ? Telinha.jamScreens : []

            PersonRow {
                id: screenRow

                required property var modelData
                required property int index

                last: index === Telinha.jamScreens.length - 1
                icon: modelData.sharing ? "screen_share" : "stop_screen_share"
                name: modelData.name
                detail: [modelData.os === "windows" ? "Windows" : "Linux", modelData.sharing ? Tr.tr("%1 watching").arg(modelData.sessions) : Tr.tr("paused")].join(" · ")

                IconButton {
                    // Para o compartilhamento (a pessoa pode retomar); expulsar fica em Espectadores.
                    visible: screenRow.modelData.sharing
                    icon: "cancel_presentation"
                    type: IconButton.Text
                    onClicked: Telinha.stopShare(screenRow.modelData.id)
                }
            }
        }

        // Espectadores
        SectionHeader {
            text: Tr.tr("Viewers")
        }

        InfoRow {
            visible: Telinha.viewers.length === 0
            first: true
            last: true
            icon: "group_off"
            label: Telinha.running ? Tr.tr("Nobody is watching yet") : Tr.tr("Not sharing")
            subtext: Tr.tr("People who open the link appear here with the name they typed")
        }

        Repeater {
            model: Telinha.viewers

            PersonRow {
                id: viewerRow

                required property var modelData
                required property int index

                first: index === 0
                last: index === Telinha.viewers.length - 1
                // Na jam, quem compartilha também aparece aqui (e pode ser expulso).
                icon: modelData.role === "presenter" ? "screen_share" : "person"
                name: modelData.name
                detail: [modelData.device, modelData.ip, root.since(modelData.since)].filter(s => s).join(" · ")

                IconButton {
                    icon: "logout"
                    type: IconButton.Text
                    onClicked: Telinha.kick(viewerRow.modelData.id)
                }

                IconButton {
                    icon: "block"
                    type: IconButton.Text
                    onClicked: Telinha.block(viewerRow.modelData.id)
                }
            }
        }

        StyledText {
            Layout.fillWidth: true
            Layout.leftMargin: Tokens.padding.small
            Layout.topMargin: Tokens.spacing.small
            visible: Telinha.viewers.length > 0
            text: Tr.tr("Kick (↪) disconnects; they can rejoin with the link. Block (⊘) also stops that browser and IP from coming back.")
            color: Colours.palette.m3outline
            font: Tokens.font.label.small
            wrapMode: Text.WordWrap
        }

        // Bloqueados
        SectionHeader {
            visible: Telinha.blocked.length > 0
            text: Tr.tr("Blocked")
        }

        Repeater {
            model: Telinha.blocked

            PersonRow {
                id: blockedRow

                required property var modelData
                required property int index

                first: index === 0
                last: index === Telinha.blocked.length - 1
                icon: "block"
                name: modelData.name
                detail: [modelData.device, modelData.ip].filter(s => s).join(" · ")

                IconButton {
                    icon: "lock_open"
                    type: IconButton.Text
                    onClicked: Telinha.unblock(blockedRow.modelData.key)
                }
            }
        }

        // Qualidade
        SectionHeader {
            text: Tr.tr("Quality")
        }

        SelectRow {
            first: true
            last: true
            label: Tr.tr("Video quality")
            subtext: {
                if (!Telinha.autoQuality)
                    return Telinha.currentQuality.detail + " · " + Tr.tr("each viewer uses this much upload; applies live");
                const now = Telinha.autoNow;
                if (!now)
                    return Tr.tr("Adjusts live to the viewers' network, your encoder and CPU use");
                return Tr.tr("Now %1 Mbps · %2 fps · %3").arg((now.kbps / 1000).toFixed(1)).arg(now.fps).arg(now.reason);
            }
            menuItems: root.qualityItems
            active: Telinha.autoQuality ? root.qualityItems[0] : (root.qualityItems[Telinha.quality + 1] ?? root.qualityItems[0])
            onSelected: item => {
                const index = root.qualityItems.indexOf(item);
                if (index === 0)
                    Telinha.setAutoQuality();
                else
                    Telinha.setQuality(index - 1);
            }
        }

        // Som
        SectionHeader {
            text: Tr.tr("Audio")
        }

        ToggleRow {
            first: true
            last: true
            text: Tr.tr("Share app audio")
            subtext: Telinha.running ? Tr.tr("Applies the next time you start sharing") : Tr.tr("Never the microphone. Voice call apps stay off to avoid echo")
            checked: Telinha.audio
            onToggled: Telinha.setAudio(checked)
        }

        StyledText {
            Layout.fillWidth: true
            Layout.leftMargin: Tokens.padding.small
            Layout.topMargin: Tokens.spacing.medium
            Layout.bottomMargin: Tokens.spacing.small
            visible: Telinha.audio
            text: Tr.tr("Apps included in the shared audio. Changes apply live.")
            color: Colours.palette.m3outline
            font: Tokens.font.body.small
            wrapMode: Text.WordWrap
        }

        InfoRow {
            visible: Telinha.audio && root.appList.length === 0
            first: true
            last: true
            icon: "music_off"
            label: Tr.tr("No apps playing audio")
            subtext: Tr.tr("Apps show up here as soon as they play something")
        }

        Repeater {
            model: Telinha.audio ? root.appList : []

            ToggleRow {
                required property var modelData
                required property int index

                first: index === 0
                last: index === root.appList.length - 1
                text: modelData.name
                subtext: modelData.voice ? Tr.tr("Voice call app · off by default so the call doesn't echo") : ""
                checked: Telinha.appEnabled(modelData.name, modelData.voice)
                onToggled: Telinha.setApp(modelData.name, checked)
            }
        }

        // Detalhes técnicos: o log da transmissão (inclui o que o navegador de
        // cada espectador relata sobre a conexão).
        SectionHeader {
            text: Tr.tr("Technical details")
        }

        RowButton {
            first: true
            last: Telinha.logLines.length === 0
            disabled: Telinha.logLines.length === 0
            icon: "content_copy"
            text: Tr.tr("Copy technical details")
            subtext: Tr.tr("Connection log of the current share, useful when someone can't watch")
            onClicked: Telinha.copyLog()
        }

        ConnectedRect {
            visible: Telinha.logLines.length > 0
            last: true
            Layout.fillWidth: true
            implicitHeight: logText.implicitHeight + Tokens.padding.medium * 2

            StyledText {
                id: logText

                anchors.fill: parent
                anchors.margins: Tokens.padding.medium
                anchors.leftMargin: Tokens.padding.largeIncreased
                text: Telinha.logLines.slice(-12).join("\n")
                color: Colours.palette.m3onSurfaceVariant
                font.family: "monospace"
                font.pointSize: Tokens.font.label.small.pointSize
                wrapMode: Text.WrapAnywhere
                textFormat: Text.PlainText
            }
        }
    }

    // Linha de pessoa (espectador ou bloqueado) com botões de ação à direita.
    component PersonRow: ConnectedRect {
        id: row

        property string icon
        property string name
        property string detail
        default property alias actions: actionRow.data

        Layout.fillWidth: true
        implicitHeight: rowLayout.implicitHeight + Tokens.padding.small * 2

        RowLayout {
            id: rowLayout

            anchors.fill: parent
            anchors.topMargin: Tokens.padding.small
            anchors.bottomMargin: Tokens.padding.small
            anchors.leftMargin: Tokens.padding.largeIncreased
            anchors.rightMargin: Tokens.padding.medium
            spacing: Tokens.spacing.medium

            MaterialIcon {
                text: row.icon
                color: Colours.palette.m3onSurfaceVariant
                fontStyle: Tokens.font.icon.medium
                fill: 1
            }

            ColumnLayout {
                Layout.fillWidth: true
                spacing: 0

                StyledText {
                    Layout.fillWidth: true
                    text: row.name
                    font: Tokens.font.body.small
                    elide: Text.ElideRight
                }

                StyledText {
                    Layout.fillWidth: true
                    visible: text
                    text: row.detail
                    color: Colours.palette.m3outline
                    font: Tokens.font.label.small
                    elide: Text.ElideRight
                }
            }

            Row {
                id: actionRow

                spacing: Tokens.spacing.extraSmall
            }
        }
    }
}
