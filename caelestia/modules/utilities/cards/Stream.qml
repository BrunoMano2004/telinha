import QtQuick
import QtQuick.Layouts
import Caelestia.Config
import Caelestia.I18n
import qs.components
import qs.components.controls
import qs.services

StyledRect {
    id: root

    readonly property bool live: Telinha.url !== ""
    readonly property real nonAnimHeight: btnLayout.implicitHeight + (live ? linkRow.implicitHeight + layout.spacing : 0) + Tokens.padding.large * 2

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
                    text: Tr.tr("Screen share")
                    font: Tokens.font.body.medium
                    elide: Text.ElideRight
                }

                StyledText {
                    Layout.fillWidth: true
                    text: {
                        if (Telinha.selecting)
                            return Tr.tr("Choose a screen...");
                        if (!root.live)
                            return Tr.tr("Ready");
                        if (Telinha.viewers === 0)
                            return Tr.tr("Live · waiting for viewers");
                        return Telinha.viewers === 1 ? Tr.tr("Live · 1 viewer") : Tr.tr("Live · %1 viewers").arg(Telinha.viewers);
                    }
                    color: Colours.palette.m3onSurfaceVariant
                    font: Tokens.font.body.small
                    elide: Text.ElideRight
                    animate: true
                }
            }

            IconButton {
                // Som do sistema junto com a tela; só muda com a transmissão parada.
                icon: Telinha.audio ? "volume_up" : "volume_off"
                isToggle: true
                checked: Telinha.audio
                disabled: Telinha.running
                type: IconButton.Tonal
                onClicked: {
                    Telinha.setAudio(!Telinha.audio);
                    internalChecked = Telinha.audio;
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
                icon: "content_copy"
                type: IconButton.Text
                onClicked: Telinha.copyLink()
            }
        }
    }

    Behavior on implicitHeight {
        Anim {}
    }
}
