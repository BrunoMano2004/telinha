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

python3 - "$tmp/modules/utilities/Content.qml" <<'EOF'
import sys
path = sys.argv[1]
s = open(path).read()

def replace(old, new):
    global s
    if old not in s:
        sys.exit(f"Content.qml mudou e o patch não se aplica mais: {old[:60]!r}")
    s = s.replace(old, new, 1)

replace("(idleInhibit.active ? 1 : 0) + ", "(idleInhibit.active ? 1 : 0) + 1 + ")
replace("((idleInhibit.item as IdleInhibit)?.nonAnimHeight ?? 0) + ",
        "((idleInhibit.item as IdleInhibit)?.nonAnimHeight ?? 0) + stream.nonAnimHeight + ")
replace("""        Loader {
            id: record
""", """        Stream {
            id: stream

            Layout.fillWidth: true
        }

        Loader {
            id: record
""")
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
