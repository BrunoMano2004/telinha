mod capture;
mod stream;

use anyhow::{Context, Result};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use gstreamer as gst;
use gstreamer::prelude::*;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use stream::Streamer;

const VIEWER_PAGE: &str = include_str!("../web/index.html");
const IDLE_STOP_DELAY: std::time::Duration = std::time::Duration::from_secs(10);

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

    /// Perfil H.264: main (padrão, aceito por todos os navegadores), high ou constrained-baseline.
    #[arg(long, default_value = "main")]
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
}

struct App {
    streamer: Streamer,
    max_viewers: usize,
    next_id: AtomicU64,
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
    let (bind, host) = resolve_bind(args.bind).await;
    let addr = SocketAddr::new(bind, args.port);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("não foi possível escutar em {addr}"))?;

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

    let streamer = Streamer::new(&description)?;
    capture::configure_encoder(&streamer.encoder, &settings);
    watch_bus(&streamer.pipeline);

    // Liga uma vez para validar captura e encoder; sem espectadores, desliga sozinho.
    let generation = streamer.start()?;
    let app = Arc::new(App { streamer, max_viewers: args.max_viewers, next_id: AtomicU64::new(1) });
    schedule_idle_stop(app.clone(), generation);

    println!("Encoder: {encoder} · {} kbps · até {} fps", args.bitrate, args.fps);
    println!("Assista em: http://{host}:{}", args.port);
    spawn_console(app.clone(), args.bitrate, args.fps);

    let router = axum::Router::new()
        .route("/", get(|| async { Html(VIEWER_PAGE) }))
        .route("/ws", get(ws_handler))
        .with_state(app.clone());

    axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(async { let _ = tokio::signal::ctrl_c().await; })
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
    State(app): State<Arc<App>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_viewer(socket, peer, app))
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

    let sender = tokio::spawn(async move {
        while let Some(text) = rx.recv().await {
            if sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

    while let Some(Ok(msg)) = incoming.next().await {
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
