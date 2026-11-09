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

## Instalação no Windows

O pacote Windows requer **Windows 11 de 64 bits**. O instalador inclui GStreamer,
encoders e Cloudflared; o usuário não precisa instalar Rust, Visual Studio ou
configurar o `PATH`. A instalação é por usuário, sem pedir administrador.

**Prévia de teste:** a captura real de monitor, os drivers de GPU e a exclusão de
chamadas ainda precisam de validação em uma máquina Windows 11.

1. Baixe e abra o [instalador Windows](https://github.com/BrunoMano2004/telinha/releases/download/v0.1.0-windows-preview.1/Telinha-Setup-0.1.0-windows-x64.exe).
2. Abra **Telinha** pelo menu Iniciar. Se quiser, fixe o aplicativo na barra de tarefas.
3. Escolha o monitor e clique em **Iniciar**. O link é copiado automaticamente.
4. Para parar, abra o ícone perto do relógio e clique em **Parar**.

Fechar a janela a recolhe para o ícone perto do relógio; a transmissão continua.
O menu desse ícone também tem Iniciar, Parar, Copiar link e Sair. A opção do
instalador para iniciar com o Windows abre somente o ícone, **sem transmitir**.
Sair encerra a transmissão e o túnel, inclusive se o servidor travar.

O **link público** vem selecionado e não precisa de Tailscale. Quem receber o
link completo pode assistir; a conexão direta de vídeo tem as mesmas limitações
de NAT descritas abaixo. Para usar somente sua tailnet, desmarque a opção e
instale/conecte o Tailscale. Nesse modo o aplicativo recusa iniciar sem Tailscale.
Se o Windows perguntar sobre acesso à rede, permita o Telinha para conectar o vídeo.

O áudio usa WASAPI por aplicativo: clientes conhecidos de chamada e seus processos
filhos ficam de fora, e **nenhum microfone é aberto**. Outros apps podem ser
excluídos pelo campo da janela ou por `--audio-exclude spotify,steam`. A seleção
acompanha os aplicativos durante a transmissão. Chamadas no navegador e áudio de
loopback que outro aplicativo já esteja reproduzindo não podem ser separados do
restante do som desse aplicativo. Não abra o próprio link com som no computador
que está transmitindo.

### Gerar o instalador

O workflow **Instalador Windows**, em GitHub Actions, compila o servidor, a interface
nativa e o instalador e testa os plugins, o servidor com vídeo/áudio sintéticos e
o encerramento. Ao terminar, o artefato **Telinha-Windows-x64** contém o `.exe` e
seu SHA-256. O workflow roda quando os arquivos relevantes são enviados ao GitHub
e também pode ser iniciado manualmente. Ele não publica uma release automaticamente.

Para compilar em um Windows de desenvolvimento, instale Rust estável (MSVC),
Visual Studio Build Tools com **Desktop development with C++** e Inno Setup 6:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\windows\build.ps1
```

O script baixa SDK/runtime do GStreamer e Cloudflared com versões e hashes fixos,
monta as dependências privadas e gera `dist\Telinha-Setup-0.1.0-windows-x64.exe`.
O GStreamer 1.26.11 foi escolhido para evitar uma regressão de captura de áudio por
processo da série 1.28.0–1.28.8. Para usar um SDK MSVC x64 já extraído, passe
`-GStreamerRoot C:\caminho\msvc_x86_64`.

**Validação em hardware:** o build automatizado não valida captura real de monitor,
drivers NVIDIA/Intel/AMD, áudio por aplicativo ou conexão entre duas redes. Antes
de distribuir, teste em Windows 11 conforme [windows/TESTING.md](windows/TESTING.md).

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
| `--profile <p>` | `constrained-baseline` | `main`/`high` comprimem um pouco melhor, mas o Firefox recusa o vídeo |
| `--encoder <nome>` | auto | `nvh264enc`, `x264enc`, … |
| `--max-viewers <n>` | 4 | |
| `--choose` | | Linux: ignora a tela salva e abre o seletor de novo |
| `-m, --monitor <n>` | 0 | Windows: índice do monitor |
| `--bind <ip>` / `-p, --port` | IP do Tailscale / 8080 | |
| `--test` | | Transmite um padrão de teste em vez da tela |
| `--tunnel` | | Link público pela Cloudflare (veja abaixo) |
| `-a, --audio` | | Transmite também o som do sistema (veja abaixo) |

Comandos no terminal enquanto roda: `b 8000` (muda o bitrate ao vivo), `k` (força
keyframe) e `s` (status).

### Link público (qualquer pessoa, sem Tailscale)

```bash
telinha --tunnel
```

Abre um *Quick Tunnel* da Cloudflare (precisa do `cloudflared`; não precisa de conta)
e mostra um link `https://….trycloudflare.com/?k=…`. O card do Caelestia
sempre usa esse modo. O túnel só existe enquanto a transmissão
estiver rodando, e a chave `k` impede que alguém assista sem o link completo.

O túnel leva só a página e a sinalização; o vídeo continua indo direto por WebRTC
(o IP público é descoberto via STUN). Em redes que bloqueiam conexões diretas
(algumas corporativas ou com NAT simétrico) o vídeo pode não conectar.

### Som

Com `--audio` (no card do Caelestia, o botão 🔊 ao lado do Start, ligado por
padrão), vai junto o som dos apps, em Opus a 128 kbps. O navegador só toca som
depois de um clique, então o vídeo abre mudo com um botão **Ativar som** (ou tecla **M**).

**Sem eco e sem microfone (Linux):** o telinha não captura a saída de áudio inteira.
Ele cria uma saída virtual própria, *telinha (transmissão)*, e liga a ela uma cópia
do som de cada app; você continua ouvindo tudo normalmente. Ficam de fora:

- apps de chamada de voz (Discord e clientes alternativos, TeamSpeak, Mumble, Zoom,
  Slack, Teams, Skype, Telegram, Signal, WhatsApp, Element, Jitsi) e qualquer fluxo
  marcado como comunicação. Assim a voz de quem está na call com você não volta
  para a pessoa;
- microfones, loopbacks e cancelamento de eco. Se algo ligar uma dessas fontes à
  saída virtual ou à captura do telinha, a ligação é desfeita na hora.

O terminal mostra o que entra e o que fica de fora (`som: + Firefox`,
`som: ignorando Discord (chamada de voz)`). Para excluir outros apps:
`--audio-exclude spotify,steam`. A saída virtual some quando a transmissão para.

Limitações: chamadas feitas *no navegador* (Meet, Discord web) saem pelo mesmo app
que o resto do navegador e não dá para separá-las. E não abra o seu próprio link
no mesmo computador com som ligado, porque o áudio da página voltaria para a transmissão.

### Compartilhar com um amigo pelo Tailscale

No painel do Tailscale, em **Machines**, abra o menu da sua máquina, escolha
**Share…** e envie o convite. Assim ele acessa só essa máquina, não a sua rede inteira.

## Requisitos

**Linux (Wayland):** GStreamer ≥ 1.22 com `gst-plugins-bad` (webrtcbin e nvcodec),
`gst-plugin-pipewire` e o `xdg-desktop-portal` do seu compositor.

**Windows 11:** use o instalador acima. Para desenvolvimento, use o GStreamer
MSVC x64 (*runtime* e *development*), com o `bin` no `PATH`, e rode
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
