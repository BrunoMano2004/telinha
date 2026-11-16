//! Jam: várias telas num link só.
//!
//! - No anfitrião (`--jam`), `/ws` recebe os navegadores e `/pub` recebe quem
//!   entra com o telinha para compartilhar; os dois conversam pela [`Room`].
//! - Em quem entra (`--join <link>`), [`run_uplink`] abre um WebSocket de saída
//!   para o `/pub` do anfitrião (sem abrir porta nem túnel) e transforma os
//!   pedidos da sala em sessões do próprio `Streamer`, via [`Publisher`].
//!
//! O vídeo vai direto (WebRTC) de quem compartilha para cada navegador; a sala
//! só repassa a sinalização, e os quadros apenas quando a conexão direta falha.

use crate::publisher::Publisher;
use crate::room::{self, Link, PresenterInfo};
use crate::stream::Layer;
use crate::viewers;
use crate::{App, HELLO_TIMEOUT, WS_DEAD_AFTER, WS_PING_INTERVAL};
use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use tokio::sync::mpsc;

/// Fila de quadros do relé por navegador (~2 s de vídeo).
const MEDIA_QUEUE: usize = 300;

const JAM_OFF: &str = "o anfitrião não liberou a jam nesta transmissão (só ele pode compartilhar agora)";

pub fn os_name() -> &'static str {
    if cfg!(target_os = "windows") { "windows" } else { "linux" }
}

/// Nome que aparece na jam quando `--name` não é dado: o do computador.
pub fn default_name() -> String {
    let name = std::env::var("COMPUTERNAME")
        .ok()
        .or_else(|| std::fs::read_to_string("/proc/sys/kernel/hostname").ok())
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_default();
    let name = name.trim();
    if name.is_empty() { "Telinha".into() } else { name.chars().take(40).collect() }
}

/// Id de navegador aceitável (o mesmo filtro do `hello`).
pub fn valid_client(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Acrescenta `key=value` à query do link.
pub fn with_param(link: &str, key: &str, value: &str) -> String {
    let sep = if link.contains('?') { '&' } else if link.ends_with('/') { '?' } else { return format!("{link}/?{key}={value}") };
    format!("{link}{sep}{key}={value}")
}

/// Link da jam (`http(s)://host[:porta]/?k=…`) → endereço do `/pub` (`ws(s)://…/pub?k=…`).
pub fn uplink_url(link: &str) -> anyhow::Result<String> {
    // Aspas e espaços de copiar o link de dentro de um comando.
    let link = link.trim().trim_matches(|c| c == '"' || c == '\'');
    let (scheme, rest) = if let Some(rest) = link.strip_prefix("https://") {
        ("wss", rest)
    } else if let Some(rest) = link.strip_prefix("http://") {
        ("ws", rest)
    } else {
        anyhow::bail!("link inválido: use o link completo da jam (http://… ou https://…)");
    };
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    anyhow::ensure!(!host.is_empty(), "link inválido: falta o endereço");
    let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
    // Só a chave interessa; `local=` e outros parâmetros da página ficam de fora.
    let key = query.split('&').find_map(|p| p.strip_prefix("k="));
    Ok(match key {
        // A chave só tem letras e números; o resto é sujeira de copiar e colar.
        Some(key) => {
            let key = percent_decode(key).unwrap_or_default();
            format!("{scheme}://{host}/pub?k={}", key.chars().filter(char::is_ascii_alphanumeric).collect::<String>())
        }
        None => format!("{scheme}://{host}/pub"),
    })
}

/// Endereço (host[:porta]) de um link http(s), só com caracteres seguros para
/// mostrar numa confirmação.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn link_host(link: &str) -> String {
    let rest = link.trim().split_once("://").map(|(_, r)| r).unwrap_or(link);
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    host.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']')).take(100).collect()
}

fn percent_decode(text: &str) -> anyhow::Result<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = text.get(i + 1..i + 3).ok_or_else(|| anyhow::anyhow!("link telinha:// malformado"))?;
            out.push(u8::from_str_radix(hex, 16).map_err(|_| anyhow::anyhow!("link telinha:// malformado"))?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Ok(String::from_utf8(out)?)
}

/// `telinha://join?link=<link da jam>&viewer=<id do navegador>` (botão
/// "Compartilhar minha tela" da página) → (link, id do navegador).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn parse_join_url(url: &str) -> anyhow::Result<(String, Option<String>)> {
    let url = url.trim();
    let rest = url
        .strip_prefix("telinha:")
        .ok_or_else(|| anyhow::anyhow!("não é um link telinha://"))?
        .trim_start_matches('/');
    let (action, query) = rest.split_once('?').unwrap_or((rest, ""));
    anyhow::ensure!(action.trim_end_matches('/') == "join", "ação desconhecida no link telinha://");
    let (mut link, mut viewer) = (None, None);
    for pair in query.split('&') {
        match pair.split_once('=') {
            Some(("link", v)) => link = Some(percent_decode(v)?),
            Some(("viewer", v)) => viewer = Some(percent_decode(v)?),
            _ => {}
        }
    }
    let link = link.ok_or_else(|| anyhow::anyhow!("o link telinha:// não diz em qual jam entrar"))?;
    anyhow::ensure!(
        link.len() <= 2048 && !link.chars().any(|c| c.is_whitespace() || c.is_control() || c == '"'),
        "link da jam inválido"
    );
    uplink_url(&link)?;
    Ok((link, viewer.filter(|v| valid_client(v))))
}

// --- Anfitrião: navegadores -------------------------------------------------------

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum FromMember {
    Watch {
        sid: u32,
        #[serde(rename = "pub")]
        presenter: u64,
        layer: Layer,
        #[serde(default)]
        mode: String,
    },
    Unwatch {
        sid: u32,
    },
    Signal {
        sid: u32,
        payload: Value,
    },
    Keyframe {
        sid: u32,
    },
    Log {
        text: String,
    },
    #[serde(other)]
    Other,
}

/// Um navegador na jam: entra na sala e pede as telas que quer ver.
pub async fn handle_member(socket: WebSocket, ip: String, app: Arc<App>) {
    let room = app.room.clone().expect("anfitrião");
    let (mut sink, mut incoming) = socket.split();
    let reply = |kind: &str| Message::Text(format!(r#"{{"type":"{kind}"}}"#).into());

    let hello = loop {
        match tokio::time::timeout(HELLO_TIMEOUT, incoming.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                if let Some(hello) = viewers::Hello::parse(&text) {
                    break hello;
                }
            }
            Ok(Some(Ok(_))) => {}
            _ => return,
        }
    };
    // Página da transmissão normal aberta de antes da jam: recarrega.
    if hello.page == "solo" {
        let _ = sink.send(reply("reload")).await;
        return;
    }
    if app.viewers.is_blocked(&hello.client, &ip) {
        let _ = sink.send(reply("blocked")).await;
        return;
    }

    let id = room.next_id();
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let (media_tx, mut media_rx) = mpsc::channel::<Vec<u8>>(MEDIA_QUEUE);
    if let Err(kind) = room.join_member(id, hello.name.clone(), hello.device.clone(), tx.clone(), media_tx) {
        let _ = sink.send(reply(kind)).await;
        return;
    }
    let label = format!("{} · {}", hello.name, if hello.device.is_empty() { "?" } else { &hello.device });
    let mut removal = app.viewers.add(id, hello, ip.clone());
    println!("+ espectador #{id} {label}{}", if ip.is_empty() { String::new() } else { format!(" ({ip})") });

    let mut sender = tokio::spawn(async move {
        let mut ping = tokio::time::interval(WS_PING_INTERVAL);
        let mut media_open = true;
        loop {
            let msg = tokio::select! {
                text = rx.recv() => match text {
                    Some(text) => Message::Text(text.into()),
                    None => break,
                },
                media = media_rx.recv(), if media_open => match media {
                    Some(data) => Message::Binary(data.into()),
                    None => { media_open = false; continue; }
                },
                _ = ping.tick() => Message::Ping(Default::default()),
            };
            if sink.send(msg).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    let mut shutdown = app.shutdown.subscribe();
    let mut jam_on = app.jam_on.subscribe();
    loop {
        let msg = tokio::select! {
            _ = shutdown.wait_for(|stopping| *stopping) => {
                // O anfitrião está saindo: avisa antes de fechar (sem tentar reconectar).
                let _ = tx.send(r#"{"type":"ended"}"#.into());
                break;
            }
            // Jam desligada: a página recarrega como transmissão normal.
            _ = jam_on.wait_for(|on| !*on) => {
                let _ = tx.send(r#"{"type":"reload"}"#.into());
                break;
            }
            why = &mut removal => {
                if let Ok(why) = why {
                    let kind = if why == viewers::Removal::Blocked { "blocked" } else { "kicked" };
                    let _ = tx.send(format!(r#"{{"type":"{kind}"}}"#));
                }
                break;
            }
            msg = tokio::time::timeout(WS_DEAD_AFTER, incoming.next()) => match msg {
                Ok(Some(Ok(msg))) => msg,
                _ => break,
            },
        };
        let text = match msg {
            Message::Text(text) => text,
            Message::Close(_) => break,
            _ => continue,
        };
        match serde_json::from_str::<FromMember>(&text) {
            Ok(FromMember::Watch { sid, presenter, layer, mode }) => {
                if let Err(reason) = room.watch(id, sid, presenter, layer, mode == "relay") {
                    let _ = tx.send(json!({ "type": "closed", "sid": sid, "reason": reason }).to_string());
                }
            }
            Ok(FromMember::Unwatch { sid }) => room.unwatch(id, sid),
            Ok(FromMember::Signal { sid, payload }) => room.member_signal(id, sid, &payload),
            Ok(FromMember::Keyframe { sid }) => room.member_keyframe(id, sid),
            Ok(FromMember::Log { text }) => println!("  #{id} {}", text.chars().take(500).collect::<String>()),
            Ok(FromMember::Other) => {}
            Err(e) => eprintln!("espectador #{id}: mensagem inválida: {e}"),
        }
    }

    room.leave_member(id);
    app.viewers.remove(id);
    drop(tx);
    if tokio::time::timeout(std::time::Duration::from_secs(2), &mut sender).await.is_err() {
        sender.abort();
    }
    println!("- espectador #{id} saiu");
}

// --- Anfitrião: quem entra para compartilhar ------------------------------------

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum FromPresenter {
    Signal {
        session: u64,
        payload: Value,
    },
    Closed {
        session: u64,
        #[serde(default)]
        reason: String,
    },
    Share {
        on: bool,
    },
    Log {
        text: String,
    },
    #[serde(other)]
    Other,
}

fn clean(v: &Value, key: &str, max: usize) -> String {
    v[key].as_str().unwrap_or_default().chars().filter(|c| !c.is_control()).take(max).collect::<String>().trim().to_string()
}

/// Um telinha que entrou com `--join`: a tela dele vira uma tela da jam.
pub async fn handle_presenter(socket: WebSocket, ip: String, app: Arc<App>) {
    let room = app.room.clone().expect("anfitrião");
    let (mut sink, mut incoming) = socket.split();
    let send_now = |value: Value| Message::Text(value.to_string().into());
    if !*app.jam_on.borrow() {
        let _ = sink.send(send_now(json!({ "type": "error", "text": JAM_OFF }))).await;
        let _ = sink.close().await;
        return;
    }

    let (text, hello) = loop {
        match tokio::time::timeout(HELLO_TIMEOUT, incoming.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                if let Some(hello) = viewers::Hello::parse(&text) {
                    break (text, hello);
                }
            }
            Ok(Some(Ok(_))) => {}
            _ => return,
        }
    };
    let v: Value = serde_json::from_str(&text).unwrap_or_default();
    let version = clean(&v, "version", 20);
    if v["proto"].as_u64() != Some(room::PROTO) {
        let text = format!(
            "versão incompatível: o anfitrião usa o telinha {} e você, o {}. Atualize os dois.",
            env!("CARGO_PKG_VERSION"),
            if version.is_empty() { "?" } else { &version }
        );
        let _ = sink.send(send_now(json!({ "type": "error", "text": text }))).await;
        return;
    }
    if app.viewers.is_blocked(&hello.client, &ip) {
        let _ = sink.send(send_now(json!({ "type": "blocked" }))).await;
        return;
    }
    let os = match clean(&v, "os", 10).as_str() {
        "windows" => "windows",
        _ => "linux",
    };
    let info = PresenterInfo {
        name: hello.name.clone(),
        os: os.into(),
        client: hello.client.clone(),
        audio: v["audio"].as_bool().unwrap_or(false),
        version,
    };

    let id = room.next_id();
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    if let Err(text) = room.add_presenter(id, info, Link::Remote(tx.clone()), false) {
        let _ = sink.send(send_now(json!({ "type": "error", "text": text }))).await;
        return;
    }
    if v["sharing"] == false {
        room.set_sharing(id, false);
    }
    let name = hello.name.clone();
    let mut removal = app.viewers.add(id, hello, ip.clone());
    let _ = tx.send(json!({ "type": "welcome", "id": id }).to_string());
    println!("+ tela #{id} {name} ({os}){}", if ip.is_empty() { String::new() } else { format!(" · {ip}") });

    let mut sender = tokio::spawn(async move {
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
        let _ = sink.close().await;
    });

    let mut shutdown = app.shutdown.subscribe();
    let mut jam_on = app.jam_on.subscribe();
    loop {
        let msg = tokio::select! {
            _ = shutdown.wait_for(|stopping| *stopping) => {
                // O anfitrião está saindo: avisa antes de fechar (sem tentar reconectar).
                let _ = tx.send(r#"{"type":"ended"}"#.into());
                break;
            }
            _ = jam_on.wait_for(|on| !*on) => {
                let _ = tx.send(json!({ "type": "error", "text": "o anfitrião desligou a jam" }).to_string());
                break;
            }
            why = &mut removal => {
                if let Ok(why) = why {
                    let kind = if why == viewers::Removal::Blocked { "blocked" } else { "kicked" };
                    let _ = tx.send(format!(r#"{{"type":"{kind}"}}"#));
                }
                break;
            }
            msg = tokio::time::timeout(WS_DEAD_AFTER, incoming.next()) => match msg {
                Ok(Some(Ok(msg))) => msg,
                _ => break,
            },
        };
        match msg {
            Message::Text(text) => match serde_json::from_str::<FromPresenter>(&text) {
                Ok(FromPresenter::Signal { session, payload }) => room.presenter_signal(id, session, &payload),
                Ok(FromPresenter::Closed { session, reason }) => room.presenter_closed(id, session, &reason),
                Ok(FromPresenter::Share { on }) => {
                    room.set_sharing(id, on);
                    println!("tela #{id} {name} {}", if on { "voltou a compartilhar" } else { "pausou" });
                }
                Ok(FromPresenter::Log { text }) => println!("  tela #{id} {}", text.chars().take(500).collect::<String>()),
                Ok(FromPresenter::Other) => {}
                Err(e) => eprintln!("tela #{id}: mensagem inválida: {e}"),
            },
            Message::Binary(data) => room.presenter_media(id, &data),
            Message::Close(_) => break,
            _ => {}
        }
    }

    room.remove_presenter(id);
    app.viewers.remove(id);
    drop(tx);
    if tokio::time::timeout(std::time::Duration::from_secs(2), &mut sender).await.is_err() {
        sender.abort();
    }
    println!("- tela #{id} {name} saiu");
}

// --- Quem entra: conexão com o anfitrião --------------------------------------

/// Estado do compartilhamento de quem entrou com `--join`.
pub struct Uplink {
    publisher: Arc<Publisher>,
    sharing: AtomicBool,
    /// Canal da conexão atual com o anfitrião (troca a cada reconexão).
    tx: Mutex<Option<mpsc::UnboundedSender<String>>>,
}

impl Uplink {
    pub fn new(publisher: Arc<Publisher>) -> Self {
        Self { publisher, sharing: AtomicBool::new(true), tx: Mutex::new(None) }
    }

    /// `share on|off`: pausar fecha todas as sessões; a tela some da grade.
    pub fn set_sharing(&self, on: bool) {
        self.sharing.store(on, Ordering::Relaxed);
        if !on {
            self.publisher.close_all();
        }
        if let Some(tx) = self.tx.lock().unwrap().as_ref() {
            let _ = tx.send(json!({ "type": "share", "on": on }).to_string());
        }
        println!("@share {}", if on { "on" } else { "off" });
    }
}

/// Como a conexão com o anfitrião terminou.
enum Ended {
    /// Caiu (rede, anfitrião reiniciando): tenta de novo.
    Lost,
    /// Fim definitivo (expulso, jam acabou, versão incompatível...).
    Final(String),
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
enum FromRoom {
    Welcome {
        id: u64,
    },
    Open {
        session: u64,
        layer: Layer,
        #[serde(default)]
        mode: String,
    },
    Close {
        session: u64,
    },
    Signal {
        session: u64,
        payload: Value,
    },
    Keyframe {
        session: u64,
    },
    StopShare,
    Kicked,
    Blocked,
    Ended,
    Error {
        text: String,
    },
    #[serde(other)]
    Other,
}

/// Mantém a conexão com o anfitrião enquanto o telinha roda; reconecta se
/// ela cair. Retorna quando a participação acaba de vez.
/// `own_page`: abrir a página da jam (`watch` no `@jam`). Falso quando quem
/// pediu para entrar foi a própria página, pelo link telinha://.
pub async fn run_uplink(
    app: Arc<App>,
    link: String,
    name: String,
    client: String,
    audio: bool,
    own_page: bool,
) -> anyhow::Result<()> {
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let uplink = app.uplink.clone().expect("modo --join");
    let url = uplink_url(&link)?;
    let watch = if own_page { with_param(&link, "local", &client) } else { String::new() };
    let mut connected_once = false;
    let mut backoff = std::time::Duration::from_secs(1);
    let mut shutdown = app.shutdown.subscribe();

    loop {
        let attempt = tokio::select! {
            _ = shutdown.wait_for(|s| *s) => return Ok(()),
            r = tokio_tungstenite::connect_async(url.as_str()) => r,
        };
        let ws = match attempt {
            Ok((ws, _)) => ws,
            Err(e) if !connected_once => anyhow::bail!("não foi possível entrar na jam ({url}): {e}"),
            Err(e) => {
                eprintln!("aviso: sem conexão com o anfitrião ({e}); tentando de novo em {}s", backoff.as_secs());
                tokio::select! {
                    _ = shutdown.wait_for(|s| *s) => return Ok(()),
                    _ = tokio::time::sleep(backoff) => {}
                }
                backoff = (backoff * 2).min(std::time::Duration::from_secs(15));
                continue;
            }
        };
        let (mut sink, mut incoming) = ws.split();
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let (media_tx, mut media_rx) = mpsc::channel::<Vec<u8>>(MEDIA_QUEUE);
        let hello = json!({
            "type": "hello", "role": "presenter", "proto": room::PROTO,
            "version": env!("CARGO_PKG_VERSION"), "name": name, "device": format!("telinha · {}", os_name()),
            "client": client, "os": os_name(), "audio": audio,
            "sharing": uplink.sharing.load(Ordering::Relaxed),
        });
        let _ = tx.send(hello.to_string());
        *uplink.tx.lock().unwrap() = Some(tx.clone());

        let writer = tokio::spawn(async move {
            let mut ping = tokio::time::interval(WS_PING_INTERVAL);
            loop {
                let msg = tokio::select! {
                    text = rx.recv() => match text {
                        Some(text) => WsMessage::Text(text.into()),
                        None => break,
                    },
                    media = media_rx.recv() => match media {
                        Some(data) => WsMessage::Binary(data.into()),
                        None => break,
                    },
                    _ = ping.tick() => WsMessage::Ping(Default::default()),
                };
                if sink.send(msg).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });

        let ended = loop {
            let msg = tokio::select! {
                _ = shutdown.wait_for(|s| *s) => break Ended::Final(String::new()),
                msg = tokio::time::timeout(WS_DEAD_AFTER, incoming.next()) => match msg {
                    Ok(Some(Ok(msg))) => msg,
                    _ => break Ended::Lost,
                },
            };
            let text = match msg {
                WsMessage::Text(text) => text,
                WsMessage::Close(_) => break Ended::Lost,
                _ => continue,
            };
            let msg = match serde_json::from_str::<FromRoom>(&text) {
                Ok(msg) => msg,
                Err(e) => {
                    eprintln!("mensagem inválida do anfitrião: {e}");
                    continue;
                }
            };
            match msg {
                FromRoom::Welcome { id } => {
                    connected_once = true;
                    backoff = std::time::Duration::from_secs(1);
                    println!("Na jam como tela #{id} ({name})");
                    if !watch.is_empty() {
                        println!("Assista às outras telas em: {watch}");
                    }
                    println!("@jam {}", json!({ "role": "guest", "id": id, "link": link, "watch": watch }));
                }
                FromRoom::Open { session, layer, mode } => {
                    if !uplink.sharing.load(Ordering::Relaxed) {
                        let _ = tx.send(json!({ "type": "closed", "session": session, "reason": "tela pausada" }).to_string());
                        continue;
                    }
                    let out = tx.clone();
                    let signal = Box::new(move |payload: String| {
                        let _ = out.send(format!(r#"{{"type":"signal","session":{session},"payload":{payload}}}"#));
                    });
                    let opened = uplink.publisher.open(
                        session,
                        layer,
                        mode == "relay",
                        signal,
                        media_tx.clone(),
                        session.to_be_bytes().to_vec(),
                    );
                    if let Err(e) = opened {
                        eprintln!("falha ao abrir a sessão {session}: {e:#}");
                        let _ = tx.send(json!({ "type": "closed", "session": session, "reason": "falha ao abrir o vídeo" }).to_string());
                    }
                }
                FromRoom::Close { session } => uplink.publisher.close(session),
                FromRoom::Signal { session, payload } => {
                    if let Err(e) = uplink.publisher.signal(session, &payload.to_string()) {
                        eprintln!("sessão {session}: {e:#}");
                    }
                }
                FromRoom::Keyframe { session } => uplink.publisher.keyframe(session),
                FromRoom::StopShare => {
                    println!("O anfitrião parou o seu compartilhamento (use `share on` para voltar)");
                    uplink.sharing.store(false, Ordering::Relaxed);
                    uplink.publisher.close_all();
                    println!("@share off");
                }
                FromRoom::Kicked => break Ended::Final("você foi removido da jam".into()),
                FromRoom::Blocked => break Ended::Final("o anfitrião bloqueou você".into()),
                FromRoom::Ended => break Ended::Final("a jam terminou".into()),
                FromRoom::Error { text } => break Ended::Final(text),
                FromRoom::Other => {}
            }
        };

        *uplink.tx.lock().unwrap() = None;
        uplink.publisher.close_all();
        drop(tx);
        drop(media_tx);
        if tokio::time::timeout(std::time::Duration::from_secs(2), writer).await.is_err() {
            eprintln!("aviso: a conexão com o anfitrião demorou a fechar");
        }
        match ended {
            Ended::Final(text) => {
                if !text.is_empty() {
                    println!("@jam-ended {}", json!({ "reason": text }));
                    eprintln!("{text}");
                }
                return Ok(());
            }
            Ended::Lost if !connected_once => anyhow::bail!("o anfitrião recusou a conexão (link errado ou jam encerrada?)"),
            Ended::Lost => {
                eprintln!("aviso: conexão com o anfitrião perdida; reconectando em {}s", backoff.as_secs());
                tokio::select! {
                    _ = shutdown.wait_for(|s| *s) => return Ok(()),
                    _ = tokio::time::sleep(backoff) => {}
                }
                backoff = (backoff * 2).min(std::time::Duration::from_secs(15));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uplink_url_keeps_only_the_key() {
        assert_eq!(uplink_url("https://a-b.trycloudflare.com/?k=XYZ").unwrap(), "wss://a-b.trycloudflare.com/pub?k=XYZ");
        assert_eq!(uplink_url("https://a.trycloudflare.com/?k=XYZ&local=c1").unwrap(), "wss://a.trycloudflare.com/pub?k=XYZ");
        assert_eq!(uplink_url("http://desktop.tail.ts.net:8080").unwrap(), "ws://desktop.tail.ts.net:8080/pub");
        assert_eq!(uplink_url(" http://100.1.2.3:8080/ ").unwrap(), "ws://100.1.2.3:8080/pub");
        assert!(uplink_url("desktop:8080").is_err());
        assert_eq!(uplink_url("\"https://a.trycloudflare.com/?k=XYZ\"").unwrap(), "wss://a.trycloudflare.com/pub?k=XYZ");
        assert_eq!(uplink_url("https://a.trycloudflare.com/?k=XYZ%22").unwrap(), "wss://a.trycloudflare.com/pub?k=XYZ");
    }

    #[test]
    fn join_url_from_the_page() {
        let (link, viewer) =
            parse_join_url("telinha://join?link=https%3A%2F%2Fa-b.trycloudflare.com%2F%3Fk%3DXYZ&viewer=abc-123").unwrap();
        assert_eq!(link, "https://a-b.trycloudflare.com/?k=XYZ");
        assert_eq!(viewer.as_deref(), Some("abc-123"));
        let (link, viewer) = parse_join_url("telinha:join/?link=http%3A%2F%2Fdesk.ts.net%3A8080").unwrap();
        assert_eq!(link, "http://desk.ts.net:8080");
        assert_eq!(viewer, None);
        // Id estranho é ignorado; link que não é da jam, recusado.
        assert_eq!(parse_join_url("telinha://join?link=http%3A%2F%2Fh&viewer=a%22b").unwrap().1, None);
        assert!(parse_join_url("telinha://join?link=file%3A%2F%2F%2Fetc").is_err());
        assert!(parse_join_url("telinha://join?link=https%3A%2F%2Fh%2F%20--x").is_err());
        assert!(parse_join_url("telinha://run?link=http%3A%2F%2Fh").is_err());
        assert!(parse_join_url("https://h").is_err());
        assert_eq!(link_host("https://a-b.trycloudflare.com/?k=1"), "a-b.trycloudflare.com");
        assert_eq!(link_host("http://100.1.2.3:8080"), "100.1.2.3:8080");
        assert_eq!(link_host("http://<b>x</b>/"), "bx");
    }

    #[test]
    fn with_param_appends_to_any_link() {
        assert_eq!(with_param("https://x.com/?k=1", "local", "c"), "https://x.com/?k=1&local=c");
        assert_eq!(with_param("http://h:8080", "local", "c"), "http://h:8080/?local=c");
        assert_eq!(with_param("http://h:8080/", "local", "c"), "http://h:8080/?local=c");
    }
}
