#[cfg(target_os = "linux")]
mod audio;
#[cfg(any(target_os = "windows", test))]
mod audio_policy;
mod audio_rules;
mod auto_quality;
mod jam;
mod open_url;
mod publisher;
mod room;
mod viewers;
#[cfg(target_os = "windows")]
mod windows_audio;
mod capture;
mod stream;

use anyhow::{Context, Result};
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{ConnectInfo, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use clap::Parser;
use gstreamer as gst;
use gstreamer::prelude::*;
use rand::Rng;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use stream::Streamer;

/// Tamanho máximo de uma mensagem de WebSocket: sem limite, qualquer um com o
/// link poderia encher a memória de quem transmite.
const BROWSER_MESSAGE_MAX: usize = 256 * 1024;
const PRESENTER_MESSAGE_MAX: usize = 8 * 1024 * 1024;
/// A página de quem assiste (transmissão normal e jam são a mesma página).
const PAGE: &str = include_str!("../web/index.html");
const COMMON_JS: &str = include_str!("../web/common.js");
const FAVICON_SVG: &str = include_str!("../assets/telinha.svg");
const FAVICON_ICO: &[u8] = include_bytes!("../assets/telinha.ico");
/// Tempo para a página se apresentar (nome, aparelho) antes de receber vídeo.
pub(crate) const HELLO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
const STUN_SERVER: &str = "stun://stun.l.google.com:19302";
pub(crate) const WS_PING_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);
pub(crate) const WS_DEAD_AFTER: std::time::Duration = std::time::Duration::from_secs(45);

/// Compartilhamento de tela leve e de baixa latência (WebRTC + encoder de hardware).
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Endereço para escutar. Padrão: o IP do Tailscale desta máquina.
    #[arg(long)]
    bind: Option<IpAddr>,

    /// Recusa iniciar sem Tailscale quando nenhum --bind foi informado.
    #[arg(long)]
    require_tailscale: bool,

    /// Porta HTTP do viewer.
    #[arg(short, long, default_value_t = 8080)]
    port: u16,

    /// Bitrate do vídeo em kbps (cada espectador consome isso de upload).
    #[arg(short, long, default_value_t = 12000)]
    bitrate: u32,

    /// Quadros por segundo máximos.
    #[arg(short, long, default_value_t = 60)]
    fps: u32,

    /// Qualidade automática: ajusta bitrate e fps ao vivo pela rede de quem
    /// assiste, pelo encoder (GPU ou CPU) e pelo uso de CPU. Ignora -b/-f.
    #[arg(long)]
    auto_quality: bool,

    /// Perfil H.264: constrained-baseline (padrão, o único que o Firefox aceita
    /// no WebRTC), main ou high (só Chrome/Edge/Safari).
    #[arg(long, default_value = "constrained-baseline")]
    profile: String,

    /// Força um encoder específico do GStreamer (ex.: nvh264enc, x264enc).
    #[arg(long)]
    encoder: Option<String>,

    /// Número máximo de espectadores simultâneos (padrão: 4; com a jam ligada, 12).
    #[arg(long)]
    max_viewers: Option<usize>,

    /// Cria uma jam: todo mundo que entrar com o telinha (`--join`) compartilha
    /// a própria tela, e o link mostra todas as telas. Também dá para ligar
    /// e desligar durante a transmissão (comando `jam on|off`).
    #[arg(long)]
    jam: bool,

    /// Entra na jam deste link e compartilha a sua tela nela.
    #[arg(long, value_name = "LINK")]
    join: Option<String>,

    /// Seu nome na jam (padrão: o nome do computador).
    #[arg(long)]
    name: Option<String>,

    /// Com --join: id do navegador que pediu para entrar (link telinha://).
    /// A página dele esconde a sua tela e já está aberta.
    #[arg(long, hide = true, value_name = "ID")]
    viewer_id: Option<String>,

    /// Abre um link telinha://join?link=… (o botão "Compartilhar minha tela"
    /// da página da jam): pede confirmação e entra na jam.
    #[arg(long, value_name = "URL")]
    open_url: Option<String>,

    /// Jam: cada pessoa que for compartilhar espera você aceitar (comandos
    /// `accept <id>` / `deny <id>`; `approve on|off` muda ao vivo).
    #[arg(long)]
    jam_approve: bool,

    /// Jam: máximo de telas compartilhadas ao mesmo tempo (contando a sua).
    #[arg(long, default_value_t = 5)]
    max_presenters: usize,

    /// Monitor a capturar (Windows). No Linux a escolha é feita pelo seletor do sistema.
    #[arg(short, long, default_value_t = 0)]
    monitor: u32,

    /// Windows: identificador nativo do monitor (usado pelo aplicativo).
    #[arg(long)]
    monitor_handle: Option<u64>,

    /// Windows: API de captura. "auto" usa a Windows Graphics Capture quando o
    /// monitor está em HDR (a DXGI deixaria a imagem lavada) e a DXGI no resto.
    #[arg(long, default_value = "auto", value_parser = ["auto", "dxgi", "wgc"])]
    capture_api: String,

    /// Linux: ignora a tela salva e abre o seletor de novo.
    #[arg(long)]
    choose: bool,

    /// Transmite um padrão de teste em vez da tela.
    #[arg(long)]
    test: bool,

    /// Publica o link na internet por um túnel da Cloudflare (trycloudflare.com,
    /// sem conta): qualquer pessoa com o link assiste, sem estar na tailnet.
    /// O link leva uma chave aleatória. Requer o `cloudflared`.
    #[arg(long)]
    tunnel: bool,

    /// Transmite também o som dos apps, excluindo clientes conhecidos de
    /// chamada (Discord, Zoom, Teams, ...) e sem abrir o microfone.
    #[arg(short, long)]
    audio: bool,

    /// Outros apps cujo som não deve ir para a transmissão (trecho do
    /// nome, separados por vírgula). Ex.: --audio-exclude spotify,steam
    #[arg(long, value_delimiter = ',')]
    audio_exclude: Vec<String>,

    /// Apps cujo som entra mesmo sendo de chamada (nome exato, como aparece
    /// no evento @apps; separados por vírgula).
    #[arg(long, value_delimiter = ',')]
    audio_include: Vec<String>,
}

pub(crate) struct App {
    streamer: Arc<Streamer>,
    /// `--max-viewers`, se dado; senão o limite muda com a jam (ver `viewer_limit`).
    max_viewers: Option<usize>,
    /// Chave exigida em `?k=` (falta só escutando apenas em 127.0.0.1).
    key: Option<String>,
    /// Pelo túnel todo mundo chega de 127.0.0.1; o IP real vem do cabeçalho
    /// CF-Connecting-IP, que só é confiável nesse modo.
    tunnel: bool,
    pub(crate) viewers: viewers::Registry,
    bitrate: AtomicU32,
    /// Qualidade automática ligada (comando `quality auto`)?
    auto: std::sync::atomic::AtomicBool,
    /// Limites do automático para o encoder em uso.
    limits: auto_quality::Limits,
    pub(crate) shutdown: tokio::sync::watch::Sender<bool>,
    /// No anfitrião: a sala da jam (sempre existe) e o id da tela dele nela.
    pub(crate) room: Option<Arc<room::Room>>,
    /// Jam liberada? Desligada, o link é uma transmissão normal e ninguém
    /// entra para compartilhar (comando `jam on|off`).
    pub(crate) jam_on: tokio::sync::watch::Sender<bool>,
    /// Link da transmissão e o da página do próprio anfitrião (`?local=`).
    link: String,
    watch_link: String,
    local_presenter: Option<u64>,
    /// Jam, em quem entrou com `--join`.
    pub(crate) uplink: Option<Arc<jam::Uplink>>,
    #[cfg(target_os = "windows")]
    capture_api: String,
    /// Sessão do portal da tela em uso (trocada pelo comando `screen`).
    #[cfg(target_os = "linux")]
    capture: std::sync::Mutex<Option<capture::portal::Capture>>,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let args = Args::parse();
    if let Some(url) = &args.open_url {
        return open_url::run(url);
    }
    // Poucas arenas de malloc: com dezenas de threads do GStreamer, o padrão
    // do glibc (8 × núcleos) infla a RAM sem ganho real.
    #[cfg(target_os = "linux")]
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 2);
    }
    gst::init()?;
    // TLS do `--join` (wss:// pelo túnel da Cloudflare).
    let _ = rustls::crypto::ring::default_provider().install_default();
    audio_rules::init(&args.audio_exclude, &args.audio_include);
    anyhow::ensure!(!(args.jam && args.join.is_some()), "use --jam (criar uma jam) ou --join <link> (entrar), não os dois");
    let joining = args.join.clone();
    anyhow::ensure!(args.viewer_id.is_none() || joining.is_some(), "--viewer-id só vale com --join");

    let settings = capture::Settings {
        bitrate_kbps: args.bitrate,
        fps: args.fps,
        profile: args.profile.clone(),
        encoder: args.encoder.clone(),
        monitor: args.monitor,
        monitor_handle: args.monitor_handle,
        capture_api: args.capture_api.clone(),
        // A miniatura da grade da jam existe sempre, com o portão fechado: assim
        // ligar a jam no meio da transmissão já tem grade leve. Fechado, o
        // encoder dela nem inicia (nenhuma sessão de NVENC, por exemplo).
        thumbnail: true,
    };
    let encoder = capture::pick_encoder(settings.encoder.as_deref())?;
    // Na jam cada navegador é um participante; o padrão de 4 seria apertado.

    // Quem entra numa jam não escuta nada: só abre uma conexão de saída.
    // Os outros abrem a porta antes de pedir a tela, para falhar cedo se ela
    // estiver ocupada. No túnel o cloudflared faz o proxy para 127.0.0.1.
    let (listener, key, _tunnel, link) = if let Some(join) = &joining {
        jam::uplink_url(join)?;
        (None, None, None, join.trim().to_string())
    } else {
        let (bind, host) = match (args.tunnel, args.bind) {
            (true, bind) => (bind.unwrap_or(Ipv4Addr::LOCALHOST.into()), String::new()),
            (false, bind) => resolve_bind(bind, args.require_tailscale).await?,
        };
        let addr = SocketAddr::new(bind, args.port);
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("não foi possível escutar em {addr}"))?;
        // O link sempre leva uma chave aleatória: sem ela, qualquer um na mesma
        // rede (ou na tailnet) que achasse a porta veria a tela. Só quem escuta
        // apenas nesta máquina (testes, 127.0.0.1) dispensa a chave.
        let key = (args.tunnel || !bind.is_loopback()).then(|| {
            rand::rng().sample_iter(rand::distr::Alphanumeric).take(20).map(char::from).collect::<String>()
        });
        let query = key.as_ref().map(|k| format!("/?k={k}")).unwrap_or_default();
        let (tunnel, link) = if args.tunnel {
            let (child, url) = start_tunnel(args.port).await?;
            (Some(child), format!("{url}{query}"))
        } else {
            (None, format!("http://{host}:{}{query}", args.port))
        };
        (Some(listener), key, tunnel, link)
    };

    #[cfg(target_os = "linux")]
    let (_capture, description) = if args.test {
        (None, capture::test_description(&encoder, &settings))
    } else {
        let cap = capture::portal::open(args.choose).await?;
        if let Some((w, h)) = cap.size {
            println!("Capturando {w}x{h}");
        }
        let desc = capture::pipeline_description(cap.fd, cap.node, &encoder, &settings);
        (Some(cap), desc)
    };
    #[cfg(target_os = "windows")]
    let description = if args.test {
        capture::test_description(&encoder, &settings)
    } else {
        capture::pipeline_description(&encoder, &settings)
    };

    // No Linux o som passa pela saída virtual do telinha, sem chamadas de voz
    // nem microfone; ela existe enquanto `_audio` viver.
    #[cfg(target_os = "linux")]
    let _audio = if args.audio && !args.test { Some(audio::start()?) } else { None };
    #[cfg(target_os = "linux")]
    let audio_source = if args.audio && args.test {
        Some("audiotestsrc is-live=true wave=ticks".into())
    } else { _audio.as_ref().map(|a| a.source_description()) };
    #[cfg(target_os = "windows")]
    let audio_source = args.audio.then(|| {
        if args.test { "audiotestsrc is-live=true wave=ticks".into() }
        else { windows_audio::source_description() }
    });
    let description = match audio_source {
        Some(source) => format!("{description} {}", capture::audio_description(&source)),
        None => description,
    };
    // STUN para quem está fora da tailnet descobrir nosso IP público: no túnel
    // e quando a jam em que entramos é pelo link público (https).
    let public = args.tunnel || joining.as_deref().is_some_and(|l| l.trim().starts_with("https://"));
    let streamer = Arc::new(Streamer::new(&description, public.then(|| STUN_SERVER.to_string()), args.fps)?);
    #[cfg(target_os = "windows")]
    let _audio = if args.audio && !args.test {
        Some(windows_audio::start(&streamer.pipeline)?)
    } else { None };
    capture::configure_encoder(&streamer.encoder, &settings);
    if let Some(low) = streamer.low_encoder() {
        capture::configure_encoder(low, &capture::thumbnail_settings(&settings));
    }
    watch_bus(&streamer.pipeline);

    // Liga uma vez para validar captura e encoder; sem espectadores, desliga sozinho.
    let generation = streamer.start()?;
    // Id deste telinha na jam: a página aberta por ele (`?local=`) esconde a
    // própria tela. Vindo do botão da página, é o id daquele navegador.
    let client: String = match args.viewer_id.as_deref().filter(|id| jam::valid_client(id)) {
        Some(id) => id.to_string(),
        None => rand::rng().sample_iter(rand::distr::Alphanumeric).take(16).map(char::from).collect(),
    };
    let name = args.name.clone().unwrap_or_else(jam::default_name);
    let publisher = Arc::new(publisher::Publisher::new(streamer.clone()));
    // Quem transmite sempre tem a sala: a jam pode ser ligada a qualquer momento.
    let room = joining.is_none().then(|| Arc::new(room::Room::new(args.max_presenters, viewer_limit(args.max_viewers, args.jam))));
    if let Some(room) = &room {
        room.set_approve(args.jam_approve);
        room.set_active(args.jam);
    }
    let watch_link = jam::with_param(&link, "local", &client);
    let app = Arc::new(App {
        streamer,
        max_viewers: args.max_viewers,
        key,
        tunnel: args.tunnel,
        viewers: viewers::Registry::default(),
        bitrate: AtomicU32::new(args.bitrate),
        auto: std::sync::atomic::AtomicBool::new(args.auto_quality),
        limits: auto_quality::limits_for(&encoder),
        shutdown: tokio::sync::watch::channel(false).0,
        room: room.clone(),
        jam_on: tokio::sync::watch::channel(args.jam).0,
        link: link.clone(),
        watch_link: watch_link.clone(),
        local_presenter: room.as_ref().map(|r| r.next_id()),
        uplink: joining.is_some().then(|| Arc::new(jam::Uplink::new(publisher.clone()))),
        #[cfg(target_os = "linux")]
        capture: std::sync::Mutex::new(_capture),
        #[cfg(target_os = "windows")]
        capture_api: args.capture_api.clone(),
    });
    stream::schedule_idle_stop(app.streamer.clone(), generation);

    println!(
        "Encoder: {encoder} · {} kbps · até {} fps · {}",
        args.bitrate,
        args.fps,
        if args.audio { "com áudio" } else { "sem áudio" }
    );
    if args.tunnel {
        println!("Túnel ativo: o link abre para qualquer pessoa na internet");
    }
    if let (Some(room), Some(id)) = (&room, app.local_presenter) {
        let info = room::PresenterInfo {
            name: name.clone(),
            os: jam::os_name().into(),
            client: client.clone(),
            audio: args.audio,
            version: env!("CARGO_PKG_VERSION").into(),
        };
        room.add_presenter(id, info, room::Link::Local(publisher.clone()), true).map_err(anyhow::Error::msg)?;
        if args.jam {
            println!("Jam criada: quem tiver o telinha entra com `telinha --join {link}`");
            println!("Link da jam (para assistir e para entrar): {link}");
            println!("Sua página (sem a sua tela): {watch_link}");
            println!("@jam {}", serde_json::json!({ "role": "host", "id": id, "link": link, "watch": watch_link }));
        } else {
            println!("Assista em: {link}");
        }
    }
    app.viewers.emit_viewers();
    app.viewers.emit_blocked();
    spawn_auto_quality(app.clone());
    spawn_console(app.clone(), tokio::runtime::Handle::current());

    if let Some(join) = joining {
        println!("Entrando na jam…");
        let result = tokio::select! {
            r = jam::run_uplink(app.clone(), join, name, client, args.audio, args.viewer_id.is_none()) => r,
            _ = shutdown_signal() => Ok(()),
        };
        app.shutdown.send_replace(true);
        app.streamer.shutdown();
        println!("\nEncerrado.");
        // Sai já: o runtime do tokio esperaria para sempre por uma consulta de
        // estatísticas (qualidade automática) presa num ramo já desmontado.
        let code = match result {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("Error: {e:#}");
                1
            }
        };
        std::process::exit(code);
    }

    // A mesma rota serve a transmissão normal ou a jam, conforme `jam_on`.
    let router = axum::Router::new()
        .route("/", get(page))
        .route("/ws", get(ws_handler))
        .route("/pub", get(jam_presenter_ws))
        .route("/common.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], COMMON_JS) }))
        .route("/favicon.svg", get(|| async { ([(header::CONTENT_TYPE, "image/svg+xml")], FAVICON_SVG) }))
        .route("/favicon.ico", get(|| async { ([(header::CONTENT_TYPE, "image/x-icon")], FAVICON_ICO) }))
        .with_state(app.clone());

    let shutdown_app = app.clone();
    let mut shutdown_rx = app.shutdown.subscribe();
    let listener = listener.expect("porta aberta");
    axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = shutdown_signal() => {},
                _ = shutdown_rx.wait_for(|stopping| *stopping) => {},
            }
            // Na jam, cada conexão avisa que ela acabou ao ver este sinal.
            shutdown_app.shutdown.send_replace(true);
        })
        .await?;

    app.streamer.shutdown();
    println!("\nEncerrado.");
    Ok(())
}

/// Por padrão escuta só no Tailscale, para a tela não ficar exposta na LAN/internet.
async fn resolve_bind(explicit: Option<IpAddr>, require_tailscale: bool) -> Result<(IpAddr, String)> {
    if let Some(ip) = explicit {
        return Ok((ip, ip.to_string()));
    }
    let run = |args: &'static [&'static str]| async move {
        let out = tokio::process::Command::new("tailscale").args(args).output().await.ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let Some(ip) = run(&["ip", "-4"]).await.and_then(|s| s.lines().next()?.trim().parse().ok()) else {
        anyhow::ensure!(!require_tailscale, "Tailscale não encontrado ou desconectado. Conecte o Tailscale ou use a opção de link público.");
        eprintln!("aviso: Tailscale não encontrado, escutando em todas as interfaces");
        return Ok((IpAddr::V4(Ipv4Addr::UNSPECIFIED), "SEU-IP".into()));
    };
    let name = run(&["status", "--json"]).await.and_then(|s| {
        let v: serde_json::Value = serde_json::from_str(&s).ok()?;
        Some(v["Self"]["DNSName"].as_str()?.trim_end_matches('.').to_string())
    });
    Ok((ip, name.filter(|n| !n.is_empty()).unwrap_or_else(|| ip.to_string())))
}

/// Abre um Quick Tunnel da Cloudflare para a porta local. O link é aleatório,
/// não precisa de conta e deixa de existir quando o processo filho termina.
async fn start_tunnel(port: u16) -> Result<(tokio::process::Child, String)> {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let mut cmd = tokio::process::Command::new("cloudflared");
    cmd.args(["tunnel", "--no-autoupdate", "--url", &format!("http://127.0.0.1:{port}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Se o telinha morrer sem limpar (SIGKILL), o túnel cai junto.
    #[cfg(target_os = "linux")]
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    let mut child = cmd
        .spawn()
        .context("não foi possível rodar o cloudflared; reinstale o telinha ou instale o cloudflared")?;

    // O cloudflared escreve tudo no stderr: primeiro o link, depois
    // "Registered tunnel connection" quando o túnel passa a aceitar acessos.
    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    let mut url = None;
    let mut last_error = String::new();
    let ready = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while let Ok(Some(line)) = lines.next_line().await {
            if url.is_none() {
                url = line
                    .split(|c: char| c.is_whitespace() || c == '|')
                    .find(|w| w.starts_with("https://") && w.ends_with(".trycloudflare.com"))
                    .map(str::to_string);
            }
            if line.contains(" ERR ") {
                last_error = line.split(" ERR ").nth(1).unwrap_or(&line).trim().to_string();
            }
            if url.is_some() && line.contains("Registered tunnel connection") {
                return true;
            }
        }
        false
    })
    .await;

    match (ready, url) {
        (Ok(true), Some(url)) => {
            // Continua lendo para o cloudflared não travar com o pipe cheio.
            tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
            Ok((child, url))
        }
        _ => {
            let _ = child.start_kill();
            anyhow::bail!(
                "o túnel da Cloudflare não subiu: {}",
                if last_error.is_empty() { "sem resposta do cloudflared" } else { &last_error }
            );
        }
    }
}

/// Ctrl+C no terminal ou SIGTERM (o Caelestia para o processo assim).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

fn authorized(app: &App, query: &HashMap<String, String>) -> bool {
    app.key.as_ref().is_none_or(|key| query.get("k").is_some_and(|k| key_matches(key, k)))
}

/// A chave só tem letras e números: o que vier a mais é sujeira de copiar e
/// colar (a aspa do comando `telinha --join "…"`, um ponto final, um `>`),
/// e não deve virar "página não encontrada".
fn key_matches(key: &str, given: &str) -> bool {
    given.chars().filter(char::is_ascii_alphanumeric).eq(key.chars())
}

async fn page(Query(query): Query<HashMap<String, String>>, State(app): State<Arc<App>>) -> Response {
    if !authorized(&app, &query) {
        return StatusCode::NOT_FOUND.into_response();
    }
    ([(header::CACHE_CONTROL, "no-store")], Html(PAGE)).into_response()
}

/// Quantos navegadores cabem: `--max-viewers`, ou 4 numa transmissão normal
/// (cada um recebe a qualidade cheia do seu upload) e 12 na jam (a grade usa
/// miniaturas).
fn viewer_limit(explicit: Option<usize>, jam: bool) -> usize {
    explicit.unwrap_or(if jam { 12 } else { 4 })
}

impl App {
    /// `jam on|off`: com a jam desligada, o link volta a ser uma transmissão
    /// normal; quem entrou para compartilhar sai, e as páginas abertas
    /// recarregam na outra versão.
    pub(crate) fn set_jam(&self, on: bool) {
        if self.jam_on.send_replace(on) == on {
            return;
        }
        if let Some(room) = &self.room {
            room.set_max_members(viewer_limit(self.max_viewers, on));
            room.set_active(on);
        }
        if on {
            println!("Jam liberada: quem assiste pode compartilhar a tela também (`telinha --join {}`)", self.link);
            println!(
                "@jam {}",
                serde_json::json!({ "role": "host", "id": self.local_presenter, "link": self.link, "watch": self.watch_link, "toggled": true })
            );
        } else {
            println!("Jam desligada: o link voltou a ser só a sua tela");
            println!("@jam-off {{}}");
        }
    }
}

/// IP de quem conecta: pelo túnel todo mundo chega de 127.0.0.1, e o real vem
/// do cabeçalho CF-Connecting-IP (só confiável nesse modo).
fn client_ip(app: &App, peer: SocketAddr, headers: &HeaderMap) -> String {
    let ip = if app.tunnel {
        headers.get("cf-connecting-ip").and_then(|v| v.to_str().ok()).unwrap_or_default().trim().to_string()
    } else {
        peer.ip().to_string()
    };
    viewers::useful_ip(&ip)
}

async fn jam_presenter_ws(
    ws: WebSocketUpgrade,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    State(app): State<Arc<App>>,
) -> Response {
    if !authorized(&app, &query) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let ip = client_ip(&app, peer, &headers);
    // Quadros do relé passam por aqui: um quadro-chave grande cabe com folga.
    ws.max_message_size(PRESENTER_MESSAGE_MAX)
        .max_frame_size(PRESENTER_MESSAGE_MAX)
        .on_upgrade(move |socket| jam::handle_presenter(socket, ip, app))
        .into_response()
}

fn watch_bus(pipeline: &gst::Pipeline) {
    let bus = pipeline.bus().unwrap();
    std::thread::spawn(move || {
        for msg in bus.iter_timed(gst::ClockTime::NONE) {
            match msg.view() {
                gst::MessageView::Error(err) => {
                    #[cfg(target_os = "windows")]
                    if windows_audio::handle_error(err.src()) { continue; }
                    eprintln!("erro no pipeline: {} ({:?})", err.error(), err.debug());
                    std::process::exit(1);
                }
                gst::MessageView::Eos(_) => {
                    eprintln!("a captura foi encerrada");
                    std::process::exit(0);
                }
                _ => {}
            }
        }
    });
}

/// Comandos pelo stdin (terminal, card do Caelestia ou app do Windows):
///   q · k · s · b <kbps> · quality <kbps> <fps> · kick <id> · block <id>
///   unblock <chave> · screen [monitor] · app on|off <nome>
///   jam on|off · share on|off · stop-share <id> · lock on|off
fn spawn_console(app: Arc<App>, rt: tokio::runtime::Handle) {
    if app.uplink.is_some() {
        println!("Comandos: quality · share on|off · screen · s · q (sair)");
    } else {
        println!("Comandos: quality <kbps> <fps> · kick/block <id> · jam on|off · approve on|off · accept/deny <id> · stop-share <id> · lock on|off · screen · s · q (sair)");
    }
    std::thread::spawn(move || {
        // Fechar sessões da jam agenda tarefas no tokio.
        let _rt = rt.enter();
        for line in std::io::stdin().lines().map_while(Result::ok) {
            let line = line.trim();
            let (cmd, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
            let rest = rest.trim();
            let num = |s: &str| s.parse::<u64>().ok();
            match cmd {
                "q" => { app.shutdown.send_replace(true); break; }
                "k" => app.streamer.request_keyframe(),
                "stats" => {
                    for stats in app.streamer.viewer_stats() {
                        for (_, value) in stats.iter() {
                            if let Ok(s) = value.get::<gst::Structure>() {
                                println!("{s}");
                            }
                        }
                    }
                }
                "s" => println!(
                    "{} espectador(es) · {} kbps · até {} fps",
                    app.streamer.viewer_count(),
                    app.bitrate.load(Ordering::Relaxed),
                    app.streamer.max_fps()
                ),
                "quality" if rest == "auto" => {
                    app.auto.store(true, Ordering::Relaxed);
                    println!("qualidade → automática");
                }
                "b" | "quality" => {
                    let mut parts = rest.split_whitespace().map(num);
                    let kbps = parts.next().flatten().map(|v| v as u32);
                    let fps = parts.next().flatten().map(|v| v as u32).unwrap_or(app.streamer.max_fps());
                    match kbps {
                        Some(kbps) if (500..=100_000).contains(&kbps) && (1..=240).contains(&fps) => {
                            app.auto.store(false, Ordering::Relaxed);
                            app.bitrate.store(kbps, Ordering::Relaxed);
                            app.streamer.set_quality(kbps, fps);
                            emit_quality(false, kbps, fps, "escolhida");
                            println!("qualidade → {kbps} kbps · até {fps} fps");
                        }
                        _ => println!("uso: quality auto | quality <kbps 500-100000> <fps 1-240>"),
                    }
                }
                "kick" | "block" => match num(rest) {
                    Some(id) => {
                        let name = app.viewers.name(id).unwrap_or_default();
                        let done = if cmd == "kick" { app.viewers.kick(id) } else { app.viewers.block(id) };
                        if done {
                            println!("{} #{id} {name}", if cmd == "kick" { "expulso" } else { "bloqueado" });
                        } else {
                            println!("espectador #{id} não encontrado");
                        }
                    }
                    None => println!("uso: {cmd} <id>"),
                },
                "unblock" => {
                    if !app.viewers.unblock(rest) {
                        println!("bloqueio {rest} não encontrado");
                    }
                }
                "app" => match rest.split_once(char::is_whitespace) {
                    Some(("on", name)) | Some(("off", name)) if !name.trim().is_empty() => {
                        let on = rest.starts_with("on");
                        audio_rules::set(name.trim(), on);
                        println!("som: {} {}", if on { "+" } else { "-" }, name.trim());
                    }
                    _ => println!("uso: app on|off <nome>"),
                },
                "screen" => switch_screen(&app, &rt, rest),
                "share" if rest == "on" || rest == "off" => {
                    let on = rest == "on";
                    if let Some(uplink) = &app.uplink {
                        uplink.set_sharing(on);
                    } else if let (Some(room), Some(id)) = (&app.room, app.local_presenter) {
                        room.set_sharing(id, on);
                        println!("@share {rest}");
                    } else {
                        println!("share só vale numa jam");
                    }
                }
                "stop-share" => match (&app.room, num(rest)) {
                    (Some(room), Some(id)) => {
                        let name = room.presenter_name(id).unwrap_or_default();
                        if room.stop_share(id) {
                            println!("compartilhamento parado: #{id} {name}");
                        } else {
                            println!("tela #{id} não encontrada");
                        }
                    }
                    _ => println!("uso (anfitrião da jam): stop-share <id>"),
                },
                "jam" if app.room.is_some() && (rest == "on" || rest == "off") => app.set_jam(rest == "on"),
                "approve" if app.room.is_some() && (rest == "on" || rest == "off") => {
                    app.room.as_ref().unwrap().set_approve(rest == "on");
                    println!("jam: {}", if rest == "on" { "cada tela nova espera você aceitar" } else { "quem entrar compartilha na hora" });
                }
                "accept" | "deny" => match (&app.room, num(rest)) {
                    (Some(room), Some(id)) => {
                        let name = room.request_name(id).unwrap_or_default();
                        if room.decide(id, cmd == "accept") {
                            println!("{} #{id} {name}", if cmd == "accept" { "tela aceita:" } else { "tela recusada:" });
                        } else {
                            println!("pedido #{id} não encontrado");
                        }
                    }
                    _ => println!("uso (anfitrião da jam): {cmd} <id>"),
                },
                "lock" if app.room.is_some() && (rest == "on" || rest == "off") => {
                    app.room.as_ref().unwrap().set_locked(rest == "on");
                    println!("jam {}", if rest == "on" { "trancada: ninguém novo entra" } else { "aberta" });
                }
                "" => {}
                _ => println!("comandos: b · quality · kick · block · unblock · screen · app · jam · approve · accept · deny · share · stop-share · lock · s · q"),
            }
        }
    });
}

/// `@quality {...}`: qualidade em uso e, no automático, o motivo da escolha.
fn emit_quality(auto: bool, kbps: u32, fps: u32, reason: &str) {
    println!("@quality {}", serde_json::json!({ "auto": auto, "kbps": kbps, "fps": fps, "reason": reason }));
}

/// Laço da qualidade automática: a cada 2 s mede a rede dos espectadores e o
/// uso de CPU e deixa o `auto_quality::Controller` decidir.
fn spawn_auto_quality(app: Arc<App>) {
    tokio::spawn(async move {
        let mut controller: Option<auto_quality::Controller> = None;
        // Contadores do intervalo anterior por espectador (ssrc): pacotes e NACKs.
        let mut previous: HashMap<u32, (u64, u32)> = HashMap::new();
        let mut tunnel_last = app.streamer.tunnel_counters();
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as f64;
        let mut last_cpu = (auto_quality::process_cpu_seconds(), std::time::Instant::now());
        let mut ticker = tokio::time::interval(auto_quality::TICK);
        loop {
            ticker.tick().await;
            let cpu_now = (auto_quality::process_cpu_seconds(), std::time::Instant::now());
            let cpu = (cpu_now.0 - last_cpu.0) / cpu_now.1.duration_since(last_cpu.1).as_secs_f64().max(0.1) / cores;
            last_cpu = cpu_now;
            if !app.auto.load(Ordering::Relaxed) {
                controller = None;
                continue;
            }
            let controller = controller.get_or_insert_with(|| {
                let c = auto_quality::Controller::new(app.limits);
                let d = c.current();
                app.bitrate.store(d.kbps, Ordering::Relaxed);
                app.streamer.set_quality(d.kbps, d.fps);
                emit_quality(true, d.kbps, d.fps, "começando pelo padrão do encoder");
                c
            });
            let streamer_app = app.clone();
            let network = tokio::task::spawn_blocking(move || streamer_app.streamer.viewer_network()).await.unwrap_or_default();
            let mut sample = auto_quality::Sample { cpu, has_viewers: !network.is_empty(), ..Default::default() };
            let mut seen = HashMap::new();
            for (ssrc, loss, jitter, packets, nacks) in network {
                // Pedidos de retransmissão no intervalo também contam como perda.
                let (p0, n0) = previous.get(&ssrc).copied().unwrap_or((packets, nacks));
                let sent = packets.saturating_sub(p0);
                let nack_ratio = if sent > 0 { nacks.saturating_sub(n0) as f64 / sent as f64 } else { 0.0 };
                sample.loss = sample.loss.max(loss).max(nack_ratio);
                sample.jitter = sample.jitter.max(jitter);
                seen.insert(ssrc, (packets, nacks));
            }
            previous = seen;
            // Espectadores pelo túnel não mandam relatórios RTCP: a proporção de
            // quadros descartados por fila cheia faz o papel da perda.
            let (sent, dropped) = app.streamer.tunnel_counters();
            let (ds, dd) = (sent.saturating_sub(tunnel_last.0), dropped.saturating_sub(tunnel_last.1));
            tunnel_last = (sent, dropped);
            if app.streamer.tunnel_viewer_count() > 0 {
                sample.has_viewers = true;
                if ds + dd > 0 {
                    sample.loss = sample.loss.max(dd as f64 / (ds + dd) as f64);
                }
            }
            if let Some(d) = controller.step(sample) {
                app.bitrate.store(d.kbps, Ordering::Relaxed);
                app.streamer.set_quality(d.kbps, d.fps);
                emit_quality(true, d.kbps, d.fps, d.reason);
                println!("qualidade automática → {} kbps · até {} fps ({})", d.kbps, d.fps, d.reason);
            }
        }
    });
}

/// Linux: abre o seletor do portal de novo e passa a capturar o que for
/// escolhido. Quem está assistindo continua conectado.
#[cfg(target_os = "linux")]
fn switch_screen(app: &Arc<App>, rt: &tokio::runtime::Handle, _arg: &str) {
    let app = app.clone();
    rt.spawn(async move {
        println!("@screen choosing");
        let cap = match capture::portal::open(true).await {
            Ok(cap) => cap,
            Err(e) => {
                println!("@screen cancelled");
                eprintln!("aviso: a tela não foi trocada: {e:#}");
                return;
            }
        };
        let (fd, node, size) = (cap.fd, cap.node, cap.size);
        let result = app.streamer.replace_screen(|screen| {
            screen.set_property("fd", fd);
            screen.set_property("path", node.to_string());
        });
        match result {
            Ok(()) => {
                // A sessão anterior do portal só fecha depois da troca.
                *app.capture.lock().unwrap() = Some(cap);
                if let Some((w, h)) = size {
                    println!("Capturando {w}x{h}");
                }
                println!("@screen switched");
            }
            Err(e) => {
                println!("@screen cancelled");
                eprintln!("aviso: a tela não foi trocada: {e:#}");
            }
        }
    });
}

/// Windows: `screen <monitor-handle>` passa a capturar outro monitor.
#[cfg(target_os = "windows")]
fn switch_screen(app: &Arc<App>, _rt: &tokio::runtime::Handle, arg: &str) {
    let Ok(handle) = arg.parse::<u64>() else {
        println!("uso: screen <monitor-handle>");
        return;
    };
    // Cada monitor pode estar ou não em HDR: a API de captura é escolhida de novo.
    let api = capture::capture_api(&app.capture_api, Some(handle), 0);
    match app.streamer.replace_screen(|screen| {
        screen.set_property("monitor-handle", handle);
        screen.set_property_from_str("capture-api", api);
    }) {
        Ok(()) => println!("@screen switched"),
        Err(e) => {
            println!("@screen cancelled");
            eprintln!("aviso: o monitor não foi trocado: {e:#}");
        }
    }
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    State(app): State<Arc<App>>,
) -> Response {
    if !authorized(&app, &query) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let ip = client_ip(&app, peer, &headers);
    // O navegador só manda sinalização (SDP, ICE, diagnósticos): pouca coisa.
    let ws = ws.max_message_size(BROWSER_MESSAGE_MAX).max_frame_size(BROWSER_MESSAGE_MAX);
    let local = query.get("local").cloned().unwrap_or_default();
    ws.on_upgrade(move |socket| jam::handle_member(socket, ip, local, app)).into_response()
}

#[cfg(test)]
mod tests {
    use super::key_matches;

    #[test]
    fn key_survives_copy_and_paste_junk() {
        assert!(key_matches("WYsXv6597ADzL4WWJZT3", "WYsXv6597ADzL4WWJZT3"));
        assert!(key_matches("WYsXv6597ADzL4WWJZT3", "WYsXv6597ADzL4WWJZT3\""));
        assert!(key_matches("WYsXv6597ADzL4WWJZT3", "WYsXv6597ADzL4WWJZT3>."));
        assert!(!key_matches("WYsXv6597ADzL4WWJZT3", "WYsXv6597ADzL4WWJZT"));
        assert!(!key_matches("WYsXv6597ADzL4WWJZT3", "WYsXv6597ADzL4WWJZT3x"));
        assert!(!key_matches("WYsXv6597ADzL4WWJZT3", ""));
    }
}
