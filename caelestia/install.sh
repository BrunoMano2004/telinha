#!/usr/bin/env bash
# Instala o card "Screen share" no Caelestia (no pacote do AUR: telinha-caelestia).
#
# O Caelestia vem do pacote em /etc/xdg/quickshell/caelestia e não tem pontos
# de extensão, então este script cria uma cópia em ~/.config/quickshell/caelestia
# (que o Quickshell prefere à do sistema) e aplica o card nela.
# Rode de novo depois de atualizar o caelestia-shell para pegar a versão nova.
# Para desfazer: rm -rf ~/.config/quickshell/caelestia
set -euo pipefail

# readlink: no pacote, este script é chamado pelo link /usr/bin/telinha-caelestia.
here=$(dirname "$(readlink -f "$0")")
system=/etc/xdg/quickshell/caelestia
user=${XDG_CONFIG_HOME:-$HOME/.config}/quickshell/caelestia

if [[ -e $user && ! -e $user/.telinha ]]; then
    echo "$user já existe e não foi criado por este script; não vou sobrescrever." >&2
    exit 1
fi

mkdir -p "$(dirname "$user")"
tmp=$(mktemp -d "${user}.XXXX")
cp -a "$system/." "$tmp/"
cp "$here/services/Telinha.qml" "$tmp/services/"
cp "$here/modules/utilities/cards/Stream.qml" "$tmp/modules/utilities/cards/"
cp "$here/modules/nexus/TelinhaWindow.qml" "$tmp/modules/nexus/"
cp "$here/modules/nexus/pages/TelinhaPage.qml" "$tmp/modules/nexus/pages/"

python3 - "$tmp" <<'EOF'
import sys
root = sys.argv[1]
path = s = None

def patch(rel):
    global path, s
    if path:
        open(path, "w").write(s)
    path = f"{root}/{rel}"
    s = open(path).read()

def replace(old, new):
    global s
    if old not in s:
        sys.exit(f"{path.split('/caelestia')[-1]} mudou e o patch não se aplica mais: {old[:60]!r}")
    s = s.replace(old, new, 1)

# Card "Screen share" no painel de utilidades.
patch("modules/utilities/Content.qml")

replace("(idleInhibit.active ? 1 : 0) + ", "(idleInhibit.active ? 1 : 0) + 1 + ")
replace("((idleInhibit.item as IdleInhibit)?.nonAnimHeight ?? 0) + ",
        "((idleInhibit.item as IdleInhibit)?.nonAnimHeight ?? 0) + stream.nonAnimHeight + ")
replace("""        Loader {
            id: record
""", """        Stream {
            id: stream

            Layout.fillWidth: true
            screenState: root.screenState
        }

        Loader {
            id: record
""")

# Página "Screen share" no Nexus (configurações), antes de "About". As duas
# listas são paralelas: a posição da entrada é a posição do componente.
patch("modules/nexus/PageRegistry.qml")
replace("""        // About
        {""", """        // Telinha (compartilhamento de tela)
        {
            label: Tr.tr("Screen share"),
            icon: "cast",
            description: Tr.tr("Viewers, quality, shared audio"),
            category: "telinha",
            telinha: true
        },

        // About
        {""")

patch("modules/nexus/PageCompRegistry.qml")
replace("""        // About
        Component {""", """        // Telinha (compartilhamento de tela)
        Component {
            StackPage {
                Component {
                    TelinhaPage {}
                }
            }
        },

        // About
        Component {""")
open(path, "w").write(s)
EOF

touch "$tmp/.telinha"
rm -rf "$user"
mv "$tmp" "$user"
echo "Card instalado em $user."
if pgrep -f 'qs -c caelestia' >/dev/null; then
    echo "Reiniciando o shell do Caelestia"
    pkill -f 'qs -c caelestia' || true
    sleep 1
    setsid caelestia shell -d >/dev/null 2>&1 < /dev/null &
else
    echo "Inicie o shell: caelestia shell -d (ou relogue)."
fi
