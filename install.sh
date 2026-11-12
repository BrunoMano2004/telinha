#!/usr/bin/env bash
# Instalador do telinha para Arch Linux (+ card no Caelestia, se presente).
#
#   ./install.sh              instala dependências, compila e instala tudo
#   ./install.sh --no-shell   não mexe no Caelestia
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
with_shell=1
[[ ${1:-} == --no-shell ]] && with_shell=0

say() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31merro:\033[0m %s\n' "$*" >&2; exit 1; }

command -v pacman >/dev/null || die "este instalador é para Arch Linux (pacman)."

# --- Dependências do sistema ---------------------------------------------
packages=(
    base-devel pkgconf
    gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad gst-plugin-pipewire
    libnice xdg-desktop-portal pipewire
)
# Encoder H.264: NVENC vem no gst-plugins-bad; Intel/AMD usam VA-API; x264 é o fallback.
packages+=(gst-plugins-ugly)
if lspci 2>/dev/null | grep -iE 'vga|3d|display' | grep -qiwE 'intel|amd|radeon'; then
    packages+=(gst-plugin-va)
fi

missing=()
for p in "${packages[@]}"; do
    pacman -Qq "$p" &>/dev/null || pacman -Qqg "$p" &>/dev/null || missing+=("$p")
done
if ((${#missing[@]})); then
    say "Instalando dependências: ${missing[*]}"
    sudo pacman -S --needed "${missing[@]}"
fi

if ! command -v cargo >/dev/null && [[ ! -x $HOME/.cargo/bin/cargo ]]; then
    say "Instalando Rust (rustup)"
    sudo pacman -S --needed rustup
    rustup default stable
fi
export PATH="$HOME/.cargo/bin:$PATH"

command -v tailscale >/dev/null || say "Aviso: Tailscale não encontrado (sudo pacman -S tailscale). Sem ele o telinha escuta em todas as interfaces."
command -v cloudflared >/dev/null || say "Aviso: cloudflared não encontrado (sudo pacman -S cloudflared). Sem ele o link público (--tunnel) não funciona."

# --- Compilação -----------------------------------------------------------
say "Compilando"
cargo build --release --manifest-path "$here/Cargo.toml"
install -Dm755 "$here/target/release/telinha" "$HOME/.local/bin/telinha"
say "Binário instalado em ~/.local/bin/telinha"

# --- Card no Caelestia ----------------------------------------------------
if ((with_shell)) && [[ -d /etc/xdg/quickshell/caelestia ]]; then
    say "Instalando o card 'Screen share' no Caelestia"
    "$here/caelestia/install.sh"
fi

say "Pronto! Rode 'telinha' ou use o botão Start no painel de utilidades."
