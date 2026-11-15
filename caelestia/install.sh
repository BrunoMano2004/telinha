#!/usr/bin/env bash
# Instala o card "Screen share" no Caelestia (no pacote do AUR: telinha-caelestia).
#
#   telinha-caelestia            instala ou atualiza
#   telinha-caelestia --remove   tira o telinha do Caelestia
#
# O Caelestia vem do pacote em /etc/xdg/quickshell/caelestia e não tem pontos
# de extensão; o Quickshell prefere a cópia em ~/.config/quickshell/caelestia.
# Se essa cópia não existir, ela é criada a partir do sistema. Se existir (com
# módulos seus, por exemplo), é usada como está: o telinha só copia os arquivos
# dele e aplica as alterações dele, sempre marcadas com "// >>> telinha" …
# "// <<< telinha" (ou "/* telinha */" no meio de uma linha). Rodar de novo
# remove a versão anterior dessas alterações e aplica a nova; nada além disso
# é tocado. Antes de mexer, os arquivos alterados vão para
# ~/.local/state/telinha/backups/.
set -euo pipefail

# readlink: no pacote, este script é chamado pelo link /usr/bin/telinha-caelestia.
here=$(dirname "$(readlink -f "$0")")
system=/etc/xdg/quickshell/caelestia
user=${XDG_CONFIG_HOME:-$HOME/.config}/quickshell/caelestia
mode=install
[[ ${1:-} == --remove ]] && mode=remove

if [[ ! -d $user ]]; then
    if [[ $mode == remove ]]; then
        echo "Não há cópia do Caelestia em $user; nada a remover."
        exit 0
    fi
    mkdir -p "$(dirname "$user")"
    cp -a "$system" "$user"
    echo "Cópia do Caelestia criada em $user."
fi

# Arquivos do telinha (destino relativo à cópia do Caelestia).
ours=(
    services/Telinha.qml
    modules/utilities/cards/Stream.qml
    modules/nexus/TelinhaWindow.qml
    modules/nexus/pages/TelinhaPage.qml
)
patched=(
    modules/utilities/Content.qml
    modules/nexus/PageRegistry.qml
    modules/nexus/PageCompRegistry.qml
)

backup=${XDG_STATE_HOME:-$HOME/.local/state}/telinha/backups/caelestia-$(date +%Y%m%d-%H%M%S)
for rel in "${patched[@]}" "${ours[@]}"; do
    if [[ -f $user/$rel ]]; then
        mkdir -p "$backup/$(dirname "$rel")"
        cp -a "$user/$rel" "$backup/$rel"
    fi
done

python3 - "$user" "$mode" <<'EOF'
import os, re, sys
root, mode = sys.argv[1], sys.argv[2]

def block(indent, body):
    lines = body.strip("\n").splitlines()
    inner = "\n".join(indent + l if l else "" for l in lines)
    return f"{indent}// >>> telinha\n{inner}\n{indent}// <<< telinha\n\n"

def strip_ours(s):
    """Remove qualquer versão das alterações do telinha (marcadas ou antigas)."""
    # Blocos marcados (e a linha em branco que os separa do resto).
    s = re.sub(r"[ \t]*// >>> telinha\n.*?// <<< telinha\n\n?", "", s, flags=re.S)
    # Trechos no meio de uma linha.
    s = s.replace(" + 1 /* telinha */", "").replace(" + stream.nonAnimHeight /* telinha */", "")
    # Versões antigas do instalador, sem marcação.
    s = s.replace("(idleInhibit.active ? 1 : 0) + 1 + ", "(idleInhibit.active ? 1 : 0) + ")
    s = s.replace("((idleInhibit.item as IdleInhibit)?.nonAnimHeight ?? 0) + stream.nonAnimHeight + ",
                  "((idleInhibit.item as IdleInhibit)?.nonAnimHeight ?? 0) + ")
    s = re.sub(r"        Stream \{\n            id: stream\n\n            Layout\.fillWidth: true\n"
               r"(            screenState: root\.screenState\n)?        \}\n\n", "", s)
    s = re.sub(r"        // Telinha \(compartilhamento de tela\)\n        \{.*?telinha: true\n        \},\n\n", "", s, flags=re.S)
    s = re.sub(r"        // Telinha \(compartilhamento de tela\)\n        Component \{\n.*?TelinhaPage \{\}\n.*?        \},\n\n", "", s, flags=re.S)
    return s

def insert_before(s, anchor, text, name):
    if anchor not in s:
        sys.exit(f"{name}: ponto de inserção não encontrado ({anchor.strip()[:40]!r}); "
                 "o Caelestia mudou e o card não foi instalado.")
    return s.replace(anchor, text + anchor, 1)

def append_to_line(s, start, text, name):
    lines = s.split("\n")
    for i, line in enumerate(lines):
        if line.lstrip().startswith(start):
            lines[i] = line.rstrip() + text
            return "\n".join(lines)
    sys.exit(f"{name}: linha {start!r} não encontrada; o Caelestia mudou e o card não foi instalado.")

def update(rel, apply):
    path = os.path.join(root, rel)
    old = open(path).read()
    new = strip_ours(old)
    if mode == "install":
        new = apply(new)
    if new != old:
        tmp = path + ".telinha-tmp"
        with open(tmp, "w") as f:
            f.write(new)
        os.replace(tmp, path)

# Card "Screen share" no painel de utilidades, antes do gravador de tela.
def content(s):
    s = append_to_line(s, "readonly property int enabledCards:", " + 1 /* telinha */", "Content.qml")
    s = append_to_line(s, "readonly property real nonAnimHeight:", " + stream.nonAnimHeight /* telinha */", "Content.qml")
    return insert_before(s, "        Loader {\n            id: record\n", block("        ", """
Stream {
    id: stream

    Layout.fillWidth: true
    screenState: root.screenState
}"""), "Content.qml")

# Página "Screen share" no Nexus, antes de "About". As duas listas são
# paralelas: a posição da entrada é a posição do componente.
def registry(s):
    return insert_before(s, "        // About\n        {", block("        ", """
{
    label: Tr.tr("Screen share"),
    icon: "cast",
    description: Tr.tr("Viewers, quality, shared audio"),
    category: "telinha",
    telinha: true
},"""), "PageRegistry.qml")

def components(s):
    return insert_before(s, "        // About\n        Component {", block("        ", """
Component {
    StackPage {
        Component {
            TelinhaPage {}
        }
    }
},"""), "PageCompRegistry.qml")

update("modules/utilities/Content.qml", content)
update("modules/nexus/PageRegistry.qml", registry)
update("modules/nexus/PageCompRegistry.qml", components)
EOF

for rel in "${ours[@]}"; do
    if [[ $mode == install ]]; then
        install -Dm644 "$here/$rel" "$user/$rel"
    else
        rm -f "$user/$rel"
    fi
done
rm -f "$user/.telinha" # marca das versões antigas, que recriavam a pasta

if [[ $mode == install ]]; then
    echo "Card instalado em $user (backup dos arquivos alterados: $backup)."
else
    echo "Telinha removido de $user (backup: $backup)."
fi
if [[ -z ${TELINHA_NO_RESTART:-} ]] && pgrep -f 'qs -c caelestia' >/dev/null; then
    echo "Reiniciando o shell do Caelestia"
    pkill -f 'qs -c caelestia' || true
    sleep 1
    setsid caelestia shell -d >/dev/null 2>&1 < /dev/null &
else
    echo "Inicie o shell: caelestia shell -d (ou relogue)."
fi
