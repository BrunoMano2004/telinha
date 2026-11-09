#[cfg(target_os = "linux")]
mod audio;
mod capture;
mod stream;

use anyhow::{Context, Result};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use gstreamer as gst;
use gstreamer::prelude::*;
use rand::Rng;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use stream::Streamer;

const VIEWER_PAGE: &str = include_str!("../web/index.html");
const STUN_SERVER: &str = "stun://stun.l.google.com:19302";
const IDLE_STOP_DELAY: std::time::Duration = std::time::Duration::from_secs(10);
const WS_PING_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);
const WS_DEAD_AFTER: std::time::Duration = std::time::Duration::from_secs(45);

/// Compartilhamento de tela leve e de baixa latência (WebRTC + encoder de hardware).
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Endereço para escutar. Padrão: o IP do Tailscale desta máquina.
    #[arg(long)]
    bind: Option<IpAddr>,

    /// Porta HTTP do viewer.
    #[arg(short, long, default_value_t = 8080)]
    port: u16,

    /// Bitrate do vídeo em kbps (cada espectador consome isso de upload).
    #[arg(short, long, default_value_t = 12000)]
    bitrate: u32,

    /// Quadros por segundo máximos.
    #[arg(short, long, default_value_t = 60)]
    fps: u32,

    /// Perfil H.264: constrained-baseline (padrão, o único que o Firefox aceita
    /// no WebRTC), main ou high (só Chrome/Edge/Safari).
    #[arg(long, default_value = "constrained-baseline")]
    profile: String,

    /// Força um encoder específico do GStreamer (ex.: nvh264enc, x264enc).
    #[arg(long)]
    encoder: Option<String>,

    /// Número máximo de espectadores simultâneos.
    #[arg(long, default_value_t = 4)]
    max_viewers: usize,

    /// Monitor a capturar (Windows). No Linux a escolha é feita pelo seletor do sistema.
    #[arg(short, long, default_value_t = 0)]
    monitor: u32,

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

    /// Transmite também o som dos apps. No Linux, chamadas de voz (Discord,
    /// Zoom, Teams, ...) e microfones ficam de fora, para não dar eco.
    #[arg(short, long)]
    audio: bool,

    /// Linux: outros apps cujo som não deve ir para a transmissão (trecho do
    /// nome, separados por vírgula). Ex.: --audio-exclude spotify,steam
    #[arg(long, value_delimiter = ',')]
    audio_exclude: Vec<String>,
}

struct App {
    streamer: Streamer,
    max_viewers: usize,
    next_id: AtomicU64,
    /// Chave exigida em `?k=` no modo túnel (o link *.trycloudflare.com é público).
    key: Option<String>,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let args = Args::parse();
    // Poucas arenas de malloc: com dezenas de threads do GStreamer, o padrão
    // do glibc (8 × núcleos) infla a RAM sem ganho real.
    #[cfg(target_os = "linux")]
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 2);
    }
    gst::init()?;

    let settings = capture::Settings {
        bitrate_kbps: args.bitrate,
        fps: args.fps,
        profile: args.profile.clone(),
        encoder: args.encoder.clone(),
        monitor: args.monitor,
    };
    let encoder = capture::pick_encoder(settings.encoder.as_deref())?;

    // Abre a porta antes de pedir a tela, para falhar cedo se ela estiver ocupada.
    // No túnel o cloudflared faz o proxy para 127.0.0.1; não precisa escutar fora.
    let (bind, host) = match (args.tunnel, args.bind) {
        (true, bind) => (bind.unwrap_or(Ipv4Addr::LOCALHOST.into()), String::new()),
        (false, bind) => resolve_bind(bind).await,
    };
    let addr = SocketAddr::new(bind, args.port);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("não foi possível escutar em {addr}"))?;
    let key = args.tunnel.then(|| {
        rand::rng().sample_iter(rand::distr::Alphanumeric).take(20).map(char::from).collect::<String>()
    });
    let (_tunnel, link) = match &key {
        Some(key) => {
            let (child, url) = start_tunnel(args.port).await?;
            (Some(child), format!("{url}/?k={key}"))
        }
        None => (None, format!("http://{host}:{}", args.port)),
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
    let _audio = if args.audio { Some(audio::start(&args.audio_exclude)?) } else { None };
    #[cfg(target_os = "linux")]
    let audio_source = _audio.as_ref().map(|a| a.source_description());
    #[cfg(target_os = "windows")]
    let audio_source = args.audio.then(|| capture::windows_audio_source(args.test));
    let description = match audio_source {
        Some(source) => format!("{description} {}", capture::audio_description(&source)),
        None => description,
    };
    let streamer = Streamer::new(&description, args.tunnel.then(|| STUN_SERVER.to_string()))?;
    capture::configure_encoder(&streamer.encoder, &settings);
    watch_bus(&streamer.pipeline);

    // Liga uma vez para validar captura e encoder; sem espectadores, desliga sozinho.
    let generation = streamer.start()?;
    let app = Arc::new(App { streamer, max_viewers: args.max_viewers, next_id: AtomicU64::new(1), key });
    schedule_idle_stop(app.clone(), generation);

    println!(
        "Encoder: {encoder} · {} kbps · até {} fps · {}",
        args.bitrate,
        args.fps,
        if args.audio { "com áudio" } else { "sem áudio" }
    );
    if args.tunnel {
        println!("Túnel ativo: o link abre para qualquer pessoa na internet");
    }
    println!("Assista em: {link}");
    spawn_console(app.clone(), args.bitrate, args.fps);

    let router = axum::Router::new()
        .route("/", get(viewer_page))
        .route("/ws", get(ws_handler))
        .with_state(app.clone());

    axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    let _ = app.streamer.pipeline.set_state(gst::State::Null);
    println!("\nEncerrado.");
    Ok(())
}

/// Por padrão escuta só no Tailscale, para a tela não ficar exposta na LAN/internet.
async fn resolve_bind(explicit: Option<IpAddr>) -> (IpAddr, String) {
    if let Some(ip) = explicit {
        return (ip, ip.to_string());
    }
    let run = |args: &'static [&'static str]| async move {
        let out = tokio::process::Command::new("tailscale").args(args).output().await.ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let Some(ip) = run(&["ip", "-4"]).await.and_then(|s| s.lines().next()?.trim().parse().ok()) else {
        eprintln!("aviso: Tailscale não encontrado, escutando em todas as interfaces");
        return (IpAddr::V4(Ipv4Addr::UNSPECIFIED), "SEU-IP".into());
    };
    let name = run(&["status", "--json"]).await.and_then(|s| {
        let v: serde_json::Value = serde_json::from_str(&s).ok()?;
        Some(v["Self"]["DNSName"].as_str()?.trim_end_matches('.').to_string())
    });
    (ip, name.filter(|n| !n.is_empty()).unwrap_or_else(|| ip.to_string()))
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
        .context("não foi possível rodar o cloudflared (sudo pacman -S cloudflared)")?;

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
    app.key.as_ref().is_none_or(|key| query.get("k") == Some(key))
}

async fn viewer_page(Query(query): Query<HashMap<String, String>>, State(app): State<Arc<App>>) -> Response {
    if !authorized(&app, &query) {
        return StatusCode::NOT_FOUND.into_response();
    }
    Html(VIEWER_PAGE).into_response()
}

/// Espera um pouco (cobre recarregar a página) e, se continuar sem ninguém,
/// para captura e encoder — o processo volta a ~30 MB e ~0% de CPU.
fn schedule_idle_stop(app: Arc<App>, generation: u64) {
    tokio::spawn(async move {
        tokio::time::sleep(IDLE_STOP_DELAY).await;
        if app.streamer.stop_if_idle(generation) {
            println!("ninguém assistindo · captura e encoder pausados");
        }
    });
}

fn watch_bus(pipeline: &gst::Pipeline) {
    let bus = pipeline.bus().unwrap();
    std::thread::spawn(move || {
        for msg in bus.iter_timed(gst::ClockTime::NONE) {
            match msg.view() {
                gst::MessageView::Error(err) => {
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

/// Comandos no terminal: `b 8000` muda o bitrate, `k` força keyframe.
fn spawn_console(app: Arc<App>, bitrate: u32, fps: u32) {
    println!("Comandos: b <kbps> (bitrate) · k (keyframe) · Ctrl+C (sair)");
    std::thread::spawn(move || {
        let mut current = bitrate;
        for line in std::io::stdin().lines().map_while(Result::ok) {
            let mut parts = line.split_whitespace();
            match (parts.next(), parts.next().and_then(|v| v.parse::<u32>().ok())) {
                (Some("b"), Some(kbps)) if kbps >= 500 => {
                    current = kbps;
                    capture::set_bitrate(&app.streamer.encoder, kbps, fps);
                    println!("bitrate → {kbps} kbps");
                }
                (Some("k"), _) => app.streamer.request_keyframe(),
                (Some("s"), _) => println!(
                    "{} espectador(es) · {current} kbps",
                    app.streamer.viewer_count()
                ),
                _ => println!("comandos: b <kbps> · k · s"),
            }
        }
    });
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Query(query): Query<HashMap<String, String>>,
    State(app): State<Arc<App>>,
) -> Response {
    if !authorized(&app, &query) {
        return StatusCode::NOT_FOUND.into_response();
    }
    ws.on_upgrade(move |socket| handle_viewer(socket, peer, app)).into_response()
}

async fn handle_viewer(socket: WebSocket, peer: SocketAddr, app: Arc<App>) {
    let (mut sink, mut incoming) = socket.split();

    if app.streamer.viewer_count() >= app.max_viewers {
        let _ = sink.send(Message::Text(r#"{"type":"full"}"#.into())).await;
        return;
    }

    let id = app.next_id.fetch_add(1, Ordering::Relaxed);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    if let Err(e) = app.streamer.add_viewer(id, tx) {
        eprintln!("falha ao adicionar espectador: {e:#}");
        return;
    }
    println!("+ espectador #{id} ({}) · total {}", peer.ip(), app.streamer.viewer_count());

    // Depois da negociação o WebSocket fica mudo, e o proxy do túnel da Cloudflare (e NATs
    // no caminho) derrubam conexões TCP ociosas — o que derrubava o vídeo junto.
    // O ping mantém o caminho vivo; o pong do navegador prova que ele segue lá.
    let sender = tokio::spawn(async move {
        let mut ping = tokio::time::interval(WS_PING_INTERVAL);
        loop {
            let msg = tokio::select! {
                text = rx.recv() => match text {
                    Some(text) => Message::Text(text.into()),
                    None => break,
                },
                _ = ping.tick() => Message::Ping(Default::default()),
            };
            if sink.send(msg).await.is_err() {
                break;
            }
        }
    });

    // Sem nada (nem pong) por tempo demais = conexão morta pela metade; libera a vaga.
    while let Ok(Some(Ok(msg))) = tokio::time::timeout(WS_DEAD_AFTER, incoming.next()).await {
        match msg {
            Message::Text(text) => {
                if let Err(e) = app.streamer.handle_message(id, &text) {
                    eprintln!("espectador #{id}: {e:#}");
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    sender.abort();
    if let Some(generation) = app.streamer.remove_viewer(id) {
        schedule_idle_stop(app.clone(), generation);
    }
    println!("- espectador #{id} saiu · total {}", app.streamer.viewer_count());
}
