# telinha

Compartilhamento de tela leve e de baixa latência. Quem transmite roda um binário;
quem assiste abre um link no navegador. O vídeo vai por WebRTC, direto entre as
máquinas, dentro do Tailscale.

```
Tela ─► PipeWire/DXGI ─► NVENC (H.264) ─► RTP ─► tee ─┬─► webrtcbin ─► navegador 1
                                                     └─► webrtcbin ─► navegador 2
```

- O vídeo é codificado **uma vez só**, independente de quantas pessoas assistem.
- Sem espectadores, a captura e o encoder **param sozinhos** depois de 10 s (≈0% de CPU)
  e voltam quando alguém abre o link.
- Por padrão o servidor escuta **apenas no IP do Tailscale**.

## Instalação (Arch Linux)

```bash
git clone git@github.com:BrunoMano2004/telinha.git
cd telinha
./install.sh
```

O instalador instala as dependências que faltarem (pede sudo), o Rust (se ainda não
tiver), compila e coloca o binário em `~/.local/bin/telinha`. Se o Caelestia
estiver instalado, ele também adiciona o card **Screen share** ao painel de
utilidades e reinicia o shell. Para pular essa parte, use `./install.sh --no-shell`.

Rode `./install.sh` de novo depois de um `git pull` ou de atualizar o `caelestia-shell`.

## Uso

Clique em **Start** no card *Screen share* do Caelestia. O link é copiado
automaticamente. Pelo terminal, o equivalente é:

```bash
telinha            # abre o seletor de tela do sistema
```

O terminal mostra o link (ex.: `http://desktop.tailXXXX.ts.net:8080`). Quem estiver
na sua tailnet abre o link no navegador. Na página: **F** ou duplo clique abre a
tela cheia, e **S** mostra as estatísticas (fps, bitrate, RTT, buffer e decodificador).

| Opção | Padrão | |
|---|---|---|
| `-b, --bitrate <kbps>` | 12000 | Cada espectador consome isso de **upload** |
| `-f, --fps <n>` | 60 | |
| `--profile <p>` | `main` | `high` não é aceito por todo navegador |
| `--encoder <nome>` | auto | `nvh264enc`, `x264enc`, … |
| `--max-viewers <n>` | 4 | |
| `--choose` | | Linux: ignora a tela salva e abre o seletor de novo |
| `-m, --monitor <n>` | 0 | Windows: índice do monitor |
| `--bind <ip>` / `-p, --port` | IP do Tailscale / 8080 | |
| `--test` | | Transmite um padrão de teste em vez da tela |

Comandos no terminal enquanto roda: `b 8000` (muda o bitrate ao vivo), `k` (força
keyframe) e `s` (status).

### Compartilhar com um amigo pelo Tailscale

No painel do Tailscale, em **Machines**, abra o menu da sua máquina, escolha
**Share…** e envie o convite. Assim ele acessa só essa máquina, não a sua rede inteira.

## Requisitos

**Linux (Wayland):** GStreamer ≥ 1.22 com `gst-plugins-bad` (webrtcbin e nvcodec),
`gst-plugin-pipewire` e o `xdg-desktop-portal` do seu compositor.

**Windows:** instale o GStreamer (MSVC, *runtime* e *development*) pelo
gstreamer.freedesktop.org, adicione o `bin` dele ao `PATH` e rode
`cargo build --release`. A captura usa `d3d11screencapturesrc` e o encoder
`nvd3d11h264enc`, `qsvh264enc`, `amfh264enc` ou `mfh264enc`.

## Notas técnicas

- **Hyprland:** o seletor aparece a cada execução. Para ele lembrar a tela, marque
  "Allow a restore token" no seletor ou ponha `screencopy { allow_token_by_default = true }`
  em `~/.config/hypr/xdph.conf`.
- `always-copy` no `pipewiresrc` é obrigatório: sem ele, o encoder segura os buffers
  e o portal cai para ~2 fps.
- Eventos `RECONFIGURE` vindos dos webrtcbin são descartados antes da captura;
  sem isso, cada espectador novo fazia o PipeWire renegociar o formato em loop.
- Captura DMA-BUF (zero-copy) foi testada no Hyprland 0.56 + NVIDIA 615 e o
  portal ficou sem buffers; por isso o caminho padrão copia o frame.
