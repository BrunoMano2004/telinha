pragma Singleton

import QtQuick
import Quickshell
import Caelestia.Config
import Caelestia.I18n
import qs.components
import qs.services
import qs.modules.nexus

// Abre o Nexus (configurações do Caelestia) direto na página do telinha.
// Igual ao WindowFactory do Caelestia, que não deixa escolher a página inicial.
Singleton {
    id: root

    readonly property int pageIndex: PageRegistry.pages.findIndex(p => p.telinha === true)

    function open(): void {
        nexusComp.createObject(dummy);
    }

    QtObject {
        id: dummy
    }

    Component {
        id: nexusComp

        FloatingWindow {
            id: win

            color: Colours.tPalette.m3surface
            surfaceFormat.opaque: false

            onVisibleChanged: {
                if (!visible)
                    destroy();
            }

            implicitWidth: nexus.implicitWidth
            implicitHeight: nexus.implicitHeight

            minimumSize.width: contentItem.Tokens.sizes.nexus.minWidth
            minimumSize.height: contentItem.Tokens.sizes.nexus.minHeight

            contentItem.Config.screen: screen.name
            contentItem.Tokens.screen: screen.name

            title: Tr.tr("Nexus — %1").arg(PageRegistry.pages[nexus.nState.currentPageIdx].label)

            Nexus {
                id: nexus

                anchors.fill: parent
                nState.screen: win.screen
                nState.isWindow: true
                onClose: win.destroy()
                Component.onCompleted: {
                    if (root.pageIndex >= 0)
                        nState.currentPageIdx = root.pageIndex;
                }
            }

            Behavior on color {
                CAnim {}
            }
        }
    }
}
