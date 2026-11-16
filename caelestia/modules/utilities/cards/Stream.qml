import QtQuick
import QtQuick.Layouts
import Caelestia.Config
import Caelestia.I18n
import qs.components
import qs.components.controls
import qs.services
import qs.modules.nexus

StyledRect {
    id: root

    // Para fechar o painel ao abrir as configurações.
    property ScreenState screenState

    readonly property bool live: Telinha.url !== ""
    readonly property real nonAnimHeight: btnLayout.implicitHeight + (live ? linkRow.implicitHeight + layout.spacing : 0) + (!Telinha.running ? jamRow.implicitHeight + layout.spacing : 0) + Tokens.padding.large * 2

    implicitHeight: layout.implicitHeight + Tokens.padding.large * 2

    radius: Tokens.rounding.large
    color: Colours.tPalette.m3surfaceContainer
    clip: true

    ColumnLayout {
        id: layout

        anchors.top: parent.top
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.margins: Tokens.padding.large
        spacing: Tokens.spacing.medium

        RowLayout {
            id: btnLayout

            spacing: Tokens.spacing.medium

            StyledRect {
                implicitWidth: implicitHeight
                implicitHeight: {
                    const h = icon.implicitHeight + Tokens.padding.small * 2;
                    return h - (h % 2);
                }

                radius: Tokens.rounding.full
                color: Telinha.running ? Colours.palette.m3secondary : Colours.palette.m3secondaryContainer

                MaterialIcon {
                    id: icon

                    anchors.centerIn: parent
                    text: root.live ? "cast_connected" : "cast"
                    color: Telinha.running ? Colours.palette.m3onSecondary : Colours.palette.m3onSecondaryContainer
                    fontStyle: Tokens.font.icon.large
                }
            }

            ColumnLayout {
                Layout.fillWidth: true
                spacing: 0

                StyledText {
                    Layout.fillWidth: true
                    text: Telinha.running && Telinha.inJam ? Tr.tr("Jam") : Tr.tr("Screen share")
                    font: Tokens.font.body.medium
                    elide: Text.ElideRight
                }

                StyledText {
                    Layout.fillWidth: true
                    text: {
                        if (Telinha.selecting)
                            return Tr.tr("Choose a screen...");
                        if (!root.live)
                            return Telinha.running && Telinha.mode === "join" ? Tr.tr("Joining the jam...") : Tr.tr("Ready");
                        if (Telinha.inJam) {
                            const paused = Telinha.sharePaused ? " · " + Tr.tr("your screen is paused") : "";
                            if (!Telinha.jamHost)
                                return Tr.tr("In the jam · sharing your screen") + paused;
                            const others = Telinha.jamScreens.length;
                            const watching = Telinha.room ? Telinha.room.viewers.length : 0;
                            return Tr.tr("Live · %1 other screens · %2 watching").arg(others).arg(watching) + paused;
                        }
                        const n = Telinha.viewers.length;
                        if (Telinha.switchingScreen)
                            return Tr.tr("Live · choosing a new screen...");
                        if (n === 0)
                            return Tr.tr("Live · waiting for viewers");
                        if (n === 1)
                            return Tr.tr("Live · %1 watching").arg(Telinha.viewers[0].name);
                        return Tr.tr("Live · %1 viewers").arg(n);
                    }
                    color: Colours.palette.m3onSurfaceVariant
                    font: Tokens.font.body.small
                    elide: Text.ElideRight
                    animate: true
                }
            }

            IconButton {
                // Espectadores, qualidade, som e troca de tela ficam no Nexus.
                icon: "settings"
                type: IconButton.Text
                onClicked: {
                    if (root.screenState)
                        root.screenState.utilities = false;
                    TelinhaWindow.open();
                }
            }

            IconTextButton {
                icon: Telinha.running ? "stop" : "play_arrow"
                text: Telinha.running ? Tr.tr("Stop") : Tr.tr("Start")
                type: Telinha.running ? IconTextButton.Tonal : IconTextButton.Filled
                onClicked: Telinha.running ? Telinha.stop() : Telinha.start()
            }
        }

        RowLayout {
            id: linkRow

            visible: root.live
            spacing: Tokens.spacing.small

            MaterialIcon {
                text: "link"
                color: Colours.palette.m3onSurfaceVariant
            }

            StyledText {
                Layout.fillWidth: true
                text: Telinha.url.replace(/^https?:\/\//, "")
                color: Colours.palette.m3onSurfaceVariant
                font: Tokens.font.body.small
                elide: Text.ElideMiddle
            }

            IconButton {
                // Libera ou desliga a jam nesta transmissão: quem assiste
                // pode compartilhar a tela também.
                visible: Telinha.running && Telinha.mode !== "join"
                icon: Telinha.jamHost ? "group_off" : "group_add"
                type: IconButton.Text
                onClicked: Telinha.setJam(!Telinha.jamHost)
            }

            IconButton {
                // Jam: a sua página, com as telas dos outros.
                visible: Telinha.inJam && !!Telinha.jam?.watch
                icon: "open_in_new"
                type: IconButton.Text
                onClicked: Telinha.openJam()
            }

            IconButton {
                visible: Telinha.inJam
                icon: Telinha.sharePaused ? "play_arrow" : "pause"
                type: IconButton.Text
                onClicked: Telinha.setSharing(Telinha.sharePaused)
            }

            IconButton {
                icon: "content_copy"
                type: IconButton.Text
                onClicked: Telinha.copyLink()
            }
        }

        // Jam: várias telas num link só. Entrar usa o link da área de transferência.
        RowLayout {
            id: jamRow

            visible: !Telinha.running
            spacing: Tokens.spacing.small

            IconTextButton {
                Layout.fillWidth: true
                icon: "groups"
                text: Tr.tr("Create jam")
                type: IconTextButton.Tonal
                onClicked: Telinha.startJam()
            }

            IconTextButton {
                Layout.fillWidth: true
                icon: "login"
                text: Tr.tr("Join copied jam")
                type: IconTextButton.Tonal
                onClicked: Telinha.joinJam()
            }
        }
    }

    Behavior on implicitHeight {
        Anim {}
    }
}
