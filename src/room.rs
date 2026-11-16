//! Sala da jam, no anfitrião: quem está assistindo (navegadores), quem está
//! compartilhando (a tela do anfitrião e quem entrou com `--join`) e as
//! sessões entre eles. A sala só repassa sinalização (SDP/ICE): o vídeo vai
//! direto de quem compartilha para o navegador. Quando a conexão direta não
//! fecha, os quadros já codificados passam por aqui (relé), sem decodificar.
//!
//! Cada navegador numera as próprias sessões (`sid`); a sala dá a cada uma um
//! id global (`session`), que é o que o apresentador conhece. Assim ninguém
//! consegue mexer na sessão de outra pessoa.

use crate::publisher::Publisher;
use crate::stream::Layer;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// Versão do protocolo entre o anfitrião e quem entra com `--join`.
pub const PROTO: u64 = 1;

/// Sessões de um navegador com a mesma tela: a que está na tela e a da troca
/// de camada (grade ↔ selecionada). Cada sessão custa um ramo WebRTC a quem
/// compartilha, então ninguém abre mais que isso.
const MAX_SESSIONS_PER_SCREEN: usize = 2;

/// Como a sala fala com quem compartilha.
pub enum Link {
    /// A tela do próprio anfitrião.
    Local(Arc<Publisher>),
    /// Quem entrou com `--join`: mensagens JSON pelo WebSocket `/pub`.
    Remote(mpsc::UnboundedSender<String>),
}

#[derive(Clone, Serialize)]
pub struct PresenterInfo {
    pub name: String,
    /// "linux" ou "windows".
    pub os: String,
    /// Id aleatório do processo do telinha; a página aberta pelo próprio
    /// apresentador (`?local=<client>`) usa isso para não mostrar a própria tela.
    pub client: String,
    pub audio: bool,
    pub version: String,
}

struct Presenter {
    info: PresenterInfo,
    sharing: bool,
    local: bool,
    link: Link,
}

/// Um navegador na sala.
pub struct MemberInfo {
    pub name: String,
    pub device: String,
    /// Id do navegador (o mesmo do bloqueio). Nunca vai para outros navegadores.
    pub client: String,
    /// `?local=` da página aberta pelo próprio telinha: o id daquele telinha.
    pub local: String,
}

struct Member {
    info: MemberInfo,
    tx: mpsc::UnboundedSender<String>,
    media: mpsc::Sender<Vec<u8>>,
}

struct Session {
    member: u64,
    sid: u32,
    presenter: u64,
    layer: Layer,
    relay: bool,
    /// Relé: a fila do navegador encheu; descarta vídeo até o próximo quadro-chave.
    dropping: bool,
}

#[derive(Default)]
struct Inner {
    members: HashMap<u64, Member>,
    presenters: HashMap<u64, Presenter>,
    sessions: HashMap<u64, Session>,
    by_sid: HashMap<(u64, u32), u64>,
}

/// O que fazer com um apresentador depois de soltar o lock da sala (a tela
/// local mexe no pipeline do GStreamer, que não deve rodar com o lock).
enum After {
    Nothing,
    LocalClose(Arc<Publisher>, u64),
}

impl After {
    fn run(self) {
        if let After::LocalClose(publisher, session) = self {
            publisher.close(session);
        }
    }
}

pub struct Room {
    inner: Mutex<Inner>,
    next: AtomicU64,
    max_presenters: usize,
    max_members: usize,
    locked: AtomicBool,
    /// Gera o apelido (`tag`) de cada tela a partir do id dela, com chaves
    /// aleatórias deste processo: estável enquanto a jam dura e sem revelar o id.
    tags: std::hash::RandomState,
}

fn signal_json(key: &str, id: u64, payload: &str) -> String {
    format!(r#"{{"type":"signal","{key}":{id},"payload":{payload}}}"#)
}

impl Room {
    pub fn new(max_presenters: usize, max_members: usize) -> Self {
        Self {
            inner: Mutex::default(),
            next: AtomicU64::new(1),
            max_presenters: max_presenters.max(1),
            max_members,
            locked: AtomicBool::new(false),
            tags: std::hash::RandomState::new(),
        }
    }

    /// Ids de navegadores, apresentadores e sessões (todos distintos).
    pub fn next_id(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }

    pub fn set_locked(&self, locked: bool) {
        self.locked.store(locked, Ordering::Relaxed);
        self.broadcast();
    }

    pub fn is_locked(&self) -> bool {
        self.locked.load(Ordering::Relaxed)
    }

    // --- Navegadores -----------------------------------------------------------

    /// Entra um navegador. Erro = tipo da mensagem de recusa ("full", "locked").
    pub fn join_member(
        &self,
        id: u64,
        info: MemberInfo,
        tx: mpsc::UnboundedSender<String>,
        media: mpsc::Sender<Vec<u8>>,
    ) -> Result<(), &'static str> {
        let mut inner = self.inner.lock().unwrap();
        if self.is_locked() {
            return Err("locked");
        }
        if inner.members.len() >= self.max_members {
            return Err("full");
        }
        inner.members.insert(id, Member { info, tx, media });
        drop(inner);
        self.broadcast();
        Ok(())
    }

    pub fn leave_member(&self, id: u64) {
        let mut inner = self.inner.lock().unwrap();
        if inner.members.remove(&id).is_none() {
            return;
        }
        let sessions: Vec<u64> = inner.sessions.iter().filter(|(_, s)| s.member == id).map(|(&k, _)| k).collect();
        let after: Vec<After> = sessions.into_iter().map(|s| Self::drop_session(&mut inner, s, None)).collect();
        drop(inner);
        after.into_iter().for_each(After::run);
        self.broadcast();
    }

    /// O navegador quer ver a tela `presenter` na camada `layer`.
    pub fn watch(&self, member: u64, sid: u32, presenter: u64, layer: Layer, relay: bool) -> Result<(), &'static str> {
        let mut inner = self.inner.lock().unwrap();
        let Some(m) = inner.members.get(&member) else { return Err("navegador desconhecido") };
        let (tx, media) = (m.tx.clone(), m.media.clone());
        if inner.by_sid.contains_key(&(member, sid)) {
            return Err("sessão já aberta");
        }
        let Some(p) = inner.presenters.get(&presenter) else { return Err("essa tela saiu da jam") };
        if !p.sharing {
            return Err("essa tela está pausada");
        }
        let open = inner.sessions.values().filter(|s| s.member == member && s.presenter == presenter).count();
        if open >= MAX_SESSIONS_PER_SCREEN {
            return Err("sessões demais com essa tela");
        }
        let session = self.next_id();
        let local = match &p.link {
            Link::Local(publisher) => Some(publisher.clone()),
            Link::Remote(link) => {
                let mode = if relay { "relay" } else { "webrtc" };
                let _ = link.send(json!({ "type": "open", "session": session, "layer": layer, "mode": mode }).to_string());
                None
            }
        };
        inner.sessions.insert(session, Session { member, sid, presenter, layer, relay, dropping: false });
        inner.by_sid.insert((member, sid), session);
        drop(inner);
        self.emit();

        if let Some(publisher) = local {
            let out = tx.clone();
            let signal = Box::new(move |payload: String| {
                let _ = out.send(signal_json("sid", u64::from(sid), &payload));
            });
            if let Err(e) = publisher.open(session, layer, relay, signal, media, sid.to_be_bytes().to_vec()) {
                eprintln!("falha ao abrir a sessão {session}: {e:#}");
                self.presenter_closed(presenter, session, "falha ao abrir o vídeo");
            }
        }
        Ok(())
    }

    pub fn unwatch(&self, member: u64, sid: u32) {
        let mut inner = self.inner.lock().unwrap();
        let Some(session) = inner.by_sid.get(&(member, sid)).copied() else { return };
        let after = Self::drop_session(&mut inner, session, None);
        drop(inner);
        after.run();
        self.emit();
    }

    /// Sinalização do navegador para a sessão `sid` dele.
    pub fn member_signal(&self, member: u64, sid: u32, payload: &Value) {
        let inner = self.inner.lock().unwrap();
        let Some(&session) = inner.by_sid.get(&(member, sid)) else { return };
        let presenter = inner.sessions[&session].presenter;
        let Some(p) = inner.presenters.get(&presenter) else { return };
        match &p.link {
            Link::Remote(link) => {
                let _ = link.send(signal_json("session", session, &payload.to_string()));
            }
            Link::Local(publisher) => {
                let publisher = publisher.clone();
                drop(inner);
                if let Err(e) = publisher.signal(session, &payload.to_string()) {
                    eprintln!("sessão {session}: {}", crate::viewers::one_line(&format!("{e:#}"), 200));
                }
            }
        }
    }

    /// Relé: o navegador perdeu quadros e pede um quadro-chave.
    pub fn member_keyframe(&self, member: u64, sid: u32) {
        let inner = self.inner.lock().unwrap();
        let Some(&session) = inner.by_sid.get(&(member, sid)) else { return };
        let presenter = inner.sessions[&session].presenter;
        match inner.presenters.get(&presenter).map(|p| &p.link) {
            Some(Link::Remote(link)) => {
                let _ = link.send(json!({ "type": "keyframe", "session": session }).to_string());
            }
            Some(Link::Local(publisher)) => {
                let publisher = publisher.clone();
                drop(inner);
                publisher.keyframe(session);
            }
            None => {}
        }
    }

    // --- Quem compartilha ------------------------------------------------------

    /// Entra uma tela. A do anfitrião (`local`) entra mesmo com a sala trancada.
    pub fn add_presenter(&self, id: u64, info: PresenterInfo, link: Link, local: bool) -> Result<(), String> {
        let mut inner = self.inner.lock().unwrap();
        if !local && self.is_locked() {
            return Err("a jam está trancada".into());
        }
        if inner.presenters.len() >= self.max_presenters {
            return Err(format!("a jam já tem {} telas (o máximo)", self.max_presenters));
        }
        inner.presenters.insert(id, Presenter { info, sharing: true, local, link });
        drop(inner);
        self.broadcast();
        Ok(())
    }

    pub fn remove_presenter(&self, id: u64) {
        let mut inner = self.inner.lock().unwrap();
        let Some(presenter) = inner.presenters.remove(&id) else { return };
        let sessions: Vec<u64> = inner.sessions.iter().filter(|(_, s)| s.presenter == id).map(|(&k, _)| k).collect();
        for session in sessions {
            Self::drop_session(&mut inner, session, Some("saiu da jam"));
        }
        drop(inner);
        if let Link::Local(publisher) = presenter.link {
            publisher.close_all();
        }
        self.broadcast();
    }

    /// Liga ou pausa uma tela. Pausar fecha as sessões dela.
    pub fn set_sharing(&self, id: u64, on: bool) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let Some(p) = inner.presenters.get_mut(&id) else { return false };
        if p.sharing == on {
            return true;
        }
        p.sharing = on;
        let after: Vec<After> = if on {
            Vec::new()
        } else {
            let sessions: Vec<u64> = inner.sessions.iter().filter(|(_, s)| s.presenter == id).map(|(&k, _)| k).collect();
            sessions.into_iter().map(|s| Self::drop_session(&mut inner, s, Some("pausada"))).collect()
        };
        drop(inner);
        after.into_iter().for_each(After::run);
        self.broadcast();
        true
    }

    /// O anfitrião para o compartilhamento de alguém (a pessoa pode retomar).
    pub fn stop_share(&self, id: u64) -> bool {
        if let Some(Link::Remote(link)) = self.inner.lock().unwrap().presenters.get(&id).map(|p| &p.link) {
            let _ = link.send(r#"{"type":"stop-share"}"#.into());
        }
        self.set_sharing(id, false)
    }

    pub fn presenter_name(&self, id: u64) -> Option<String> {
        self.inner.lock().unwrap().presenters.get(&id).map(|p| p.info.name.clone())
    }

    /// Sinalização do apresentador para uma sessão dele.
    pub fn presenter_signal(&self, presenter: u64, session: u64, payload: &Value) {
        let inner = self.inner.lock().unwrap();
        let Some(s) = inner.sessions.get(&session).filter(|s| s.presenter == presenter) else { return };
        if let Some(m) = inner.members.get(&s.member) {
            let _ = m.tx.send(signal_json("sid", u64::from(s.sid), &payload.to_string()));
        }
    }

    /// O apresentador não conseguiu abrir (ou fechou) uma sessão dele.
    pub fn presenter_closed(&self, presenter: u64, session: u64, reason: &str) {
        let mut inner = self.inner.lock().unwrap();
        if inner.sessions.get(&session).is_none_or(|s| s.presenter != presenter) {
            return;
        }
        let after = Self::drop_session(&mut inner, session, Some(reason));
        drop(inner);
        after.run();
    }

    /// Relé: quadro `[session u64][tipo][pts][dados]` vindo do apresentador,
    /// repassado ao navegador como `[sid u32][tipo][pts][dados]`. Se a fila do
    /// navegador encher, descarta vídeo até o próximo quadro-chave e pede um.
    pub fn presenter_media(&self, presenter: u64, frame: &[u8]) {
        if frame.len() < 17 {
            return;
        }
        let session = u64::from_be_bytes(frame[..8].try_into().unwrap());
        let kind = frame[8];
        let mut inner = self.inner.lock().unwrap();
        let Inner { sessions, members, presenters, .. } = &mut *inner;
        let Some(s) = sessions.get_mut(&session).filter(|s| s.presenter == presenter && s.relay) else { return };
        let Some(m) = members.get(&s.member) else { return };
        let video = kind == 1 || kind == 2;
        if s.dropping && kind == 2 {
            return;
        }
        let mut out = Vec::with_capacity(frame.len() - 4);
        out.extend_from_slice(&s.sid.to_be_bytes());
        out.extend_from_slice(&frame[8..]);
        match m.media.try_send(out) {
            Ok(()) => {
                if kind == 1 {
                    s.dropping = false;
                }
            }
            Err(_) if video => {
                if !s.dropping {
                    s.dropping = true;
                    if let Some(Presenter { link: Link::Remote(link), .. }) = presenters.get(&presenter) {
                        let _ = link.send(json!({ "type": "keyframe", "session": session }).to_string());
                    }
                }
            }
            Err(_) => {}
        }
    }

    // --- Estado ------------------------------------------------------------------

    /// Tira a sessão da sala. Com `reason`, avisa o navegador (`closed`); o
    /// apresentador remoto recebe `close`, e o local é fechado pelo `After`.
    fn drop_session(inner: &mut Inner, session: u64, reason: Option<&str>) -> After {
        let Some(s) = inner.sessions.remove(&session) else { return After::Nothing };
        inner.by_sid.remove(&(s.member, s.sid));
        if let (Some(reason), Some(m)) = (reason, inner.members.get(&s.member)) {
            let _ = m.tx.send(json!({ "type": "closed", "sid": s.sid, "reason": reason }).to_string());
        }
        match inner.presenters.get(&s.presenter).map(|p| &p.link) {
            Some(Link::Remote(link)) => {
                let _ = link.send(json!({ "type": "close", "session": session }).to_string());
                After::Nothing
            }
            Some(Link::Local(publisher)) => After::LocalClose(publisher.clone(), session),
            None => After::Nothing,
        }
    }

    /// Telas da sala. Para um navegador (`member`), cada uma vem com um
    /// apelido (`tag`) e `own` (é a tela deste navegador ou do telinha que abriu
    /// esta página), sem o id real de ninguém.
    fn presenters_json(&self, inner: &Inner, member: Option<&MemberInfo>) -> Vec<Value> {
        use std::hash::BuildHasher;
        let mut ids: Vec<&u64> = inner.presenters.keys().collect();
        ids.sort();
        ids.into_iter()
            .map(|id| {
                let p = &inner.presenters[id];
                let watchers = inner.sessions.values().filter(|s| s.presenter == *id).count();
                let mut v = json!({
                    "id": id, "name": p.info.name, "os": p.info.os,
                    "audio": p.info.audio, "sharing": p.sharing, "host": p.local, "sessions": watchers,
                });
                if let Some(m) = member {
                    let client = &p.info.client;
                    v["tag"] = format!("{:016x}", self.tags.hash_one(client)).into();
                    v["own"] = (!client.is_empty() && (*client == m.client || *client == m.local)).into();
                }
                v
            })
            .collect()
    }

    /// Manda o estado da sala a cada navegador (`room`) e às interfaces (`@room`).
    pub fn broadcast(&self) {
        self.publish(true);
    }

    /// Só o `@room` das interfaces (quem vê o quê mudou; os navegadores não precisam saber).
    fn emit(&self) {
        self.publish(false);
    }

    fn publish(&self, to_members: bool) {
        let inner = self.inner.lock().unwrap();
        let presenters = self.presenters_json(&inner, None);
        let mut members: Vec<(&u64, &Member)> = inner.members.iter().collect();
        members.sort_by_key(|(id, _)| **id);
        // Quem está vendo o quê (a tela e a camada), para as interfaces.
        let viewers: Vec<Value> = members
            .iter()
            .map(|(id, m)| {
                let mut watching: Vec<(u64, Layer)> =
                    inner.sessions.values().filter(|s| s.member == **id).map(|s| (s.presenter, s.layer)).collect();
                watching.sort_by_key(|(p, _)| *p);
                let watching: Vec<Value> = watching.into_iter().map(|(p, l)| json!({ "pub": p, "layer": l })).collect();
                json!({ "id": id, "name": m.info.name, "device": m.info.device, "watching": watching })
            })
            .collect();
        let locked = self.is_locked();
        // Cada navegador recebe só o que a página usa: as telas (com apelido)
        // e se a sala está trancada. Quem mais assiste fica só com o anfitrião.
        for (id, m) in members.iter().filter(|_| to_members) {
            let presenters = self.presenters_json(&inner, Some(&m.info));
            let msg = json!({ "type": "room", "you": id, "locked": locked, "presenters": presenters });
            let _ = m.tx.send(msg.to_string());
        }
        println!("@room {}", json!({ "locked": locked, "presenters": presenters, "viewers": viewers }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(name: &str) -> PresenterInfo {
        PresenterInfo { name: name.into(), os: "linux".into(), client: format!("c-{name}"), audio: true, version: "t".into() }
    }

    type Rx = mpsc::UnboundedReceiver<String>;

    fn minfo(name: &str) -> MemberInfo {
        MemberInfo { name: name.into(), device: "Firefox".into(), client: format!("b-{name}"), local: String::new() }
    }

    fn member(room: &Room) -> (u64, Rx, mpsc::Receiver<Vec<u8>>) {
        let id = room.next_id();
        let (tx, rx) = mpsc::unbounded_channel();
        let (media_tx, media_rx) = mpsc::channel(2);
        room.join_member(id, minfo("Ana"), tx, media_tx).unwrap();
        (id, rx, media_rx)
    }

    fn last_room(rx: &mut Rx) -> Value {
        let mut last = Value::Null;
        while let Ok(text) = rx.try_recv() {
            let v: Value = serde_json::from_str(&text).unwrap();
            if v["type"] == "room" {
                last = v;
            }
        }
        last
    }

    /// O navegador não recebe o id de ninguém nem a lista de quem assiste;
    /// só sabe qual tela é a dele (a que entrou pelo botão da página).
    #[test]
    fn browsers_get_tags_not_ids() {
        let room = Room::new(5, 10);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (media, _media_rx) = mpsc::channel(2);
        let me = MemberInfo { name: "Ana".into(), device: "x".into(), client: "c-Bia".into(), local: String::new() };
        room.join_member(room.next_id(), me, tx, media).unwrap();
        let (_other, _orx, _omedia) = member(&room);
        let (_bia, _brx) = presenter(&room, "Bia");
        let (_caio, _crx) = presenter(&room, "Caio");
        let msg = last_room(&mut rx);
        assert!(msg.get("viewers").is_none(), "{msg}");
        let text = msg.to_string();
        assert!(!text.contains("c-Bia") && !text.contains("c-Caio") && !text.contains("b-Ana"), "{text}");
        let screens = msg["presenters"].as_array().unwrap();
        let own: Vec<bool> = screens.iter().map(|p| p["own"].as_bool().unwrap()).collect();
        assert_eq!(own, vec![true, false]);
        assert_ne!(screens[0]["tag"], screens[1]["tag"]);
    }

    fn presenter(room: &Room, name: &str) -> (u64, Rx) {
        let id = room.next_id();
        let (tx, rx) = mpsc::unbounded_channel();
        room.add_presenter(id, info(name), Link::Remote(tx), false).unwrap();
        (id, rx)
    }

    fn drain(rx: &mut Rx) -> Vec<Value> {
        let mut out = Vec::new();
        while let Ok(text) = rx.try_recv() {
            out.push(serde_json::from_str(&text).unwrap());
        }
        out
    }

    fn of_type<'a>(msgs: &'a [Value], kind: &str) -> Vec<&'a Value> {
        msgs.iter().filter(|m| m["type"] == kind).collect()
    }

    #[test]
    fn watch_routes_signals_between_the_right_pair() {
        let room = Room::new(5, 10);
        let (pub_a, mut a_rx) = presenter(&room, "a");
        let (pub_b, mut b_rx) = presenter(&room, "b");
        let (m1, mut m1_rx, _) = member(&room);
        let (m2, mut m2_rx, _) = member(&room);
        room.watch(m1, 1, pub_a, Layer::Low, false).unwrap();
        let open = drain(&mut a_rx);
        let open = of_type(&open, "open");
        assert_eq!(open.len(), 1);
        assert_eq!(open[0]["layer"], "low");
        let session = open[0]["session"].as_u64().unwrap();

        // Sinal do apresentador chega só ao navegador dono da sessão, com o sid dele.
        drain(&mut m1_rx);
        drain(&mut m2_rx);
        room.presenter_signal(pub_a, session, &json!({ "type": "offer", "sdp": "x" }));
        let got = drain(&mut m1_rx);
        assert_eq!(got[0]["sid"], 1);
        assert_eq!(got[0]["payload"]["sdp"], "x");
        assert!(drain(&mut m2_rx).is_empty());

        // Outro apresentador não fala pela sessão de A.
        room.presenter_signal(pub_b, session, &json!({ "type": "offer", "sdp": "y" }));
        assert!(drain(&mut m1_rx).is_empty());

        // Outro navegador não fala pela sessão de m1 (sid é por navegador).
        room.member_signal(m2, 1, &json!({ "type": "answer", "sdp": "z" }));
        assert!(drain(&mut a_rx).is_empty());
        room.member_signal(m1, 1, &json!({ "type": "answer", "sdp": "z" }));
        let got = drain(&mut a_rx);
        assert_eq!(got[0]["session"].as_u64(), Some(session));
        assert!(drain(&mut b_rx).iter().all(|m| m["type"] != "signal"));
    }

    #[test]
    fn presenter_leaving_or_pausing_closes_its_sessions() {
        let room = Room::new(5, 10);
        let (pub_a, mut a_rx) = presenter(&room, "a");
        let (m1, mut m1_rx, _) = member(&room);
        room.watch(m1, 1, pub_a, Layer::High, false).unwrap();
        room.watch(m1, 2, pub_a, Layer::Low, false).unwrap();
        assert!(room.watch(m1, 2, pub_a, Layer::Low, false).is_err(), "sid repetido");
        assert!(room.watch(m1, 9, pub_a, Layer::Low, false).is_err(), "no máximo 2 sessões com a mesma tela");
        drain(&mut m1_rx);

        assert!(room.stop_share(pub_a));
        let closed = drain(&mut m1_rx);
        assert_eq!(of_type(&closed, "closed").len(), 2);
        let to_a = drain(&mut a_rx);
        assert_eq!(of_type(&to_a, "stop-share").len(), 1);
        assert_eq!(of_type(&to_a, "close").len(), 2);
        assert!(room.watch(m1, 3, pub_a, Layer::Low, false).is_err(), "tela pausada");

        room.set_sharing(pub_a, true);
        room.watch(m1, 3, pub_a, Layer::Low, false).unwrap();
        drain(&mut m1_rx);
        room.remove_presenter(pub_a);
        let msgs = drain(&mut m1_rx);
        assert_eq!(of_type(&msgs, "closed")[0]["sid"], 3);
        let room_msg = of_type(&msgs, "room");
        assert!(room_msg.last().unwrap()["presenters"].as_array().unwrap().is_empty());
    }

    #[test]
    fn member_leaving_closes_sessions_on_the_presenter() {
        let room = Room::new(5, 10);
        let (pub_a, mut a_rx) = presenter(&room, "a");
        let (m1, _m1_rx, _) = member(&room);
        room.watch(m1, 7, pub_a, Layer::Low, false).unwrap();
        drain(&mut a_rx);
        room.leave_member(m1);
        assert_eq!(of_type(&drain(&mut a_rx), "close").len(), 1);
    }

    #[test]
    fn limits_and_lock() {
        let room = Room::new(2, 1);
        presenter(&room, "a");
        presenter(&room, "b");
        let (tx, _rx) = mpsc::unbounded_channel();
        assert!(room.add_presenter(99, info("c"), Link::Remote(tx), false).is_err());
        member(&room);
        let (tx, _rx) = mpsc::unbounded_channel();
        let (media, _m) = mpsc::channel(1);
        assert_eq!(room.join_member(98, minfo("x"), tx, media), Err("full"));

        let room = Room::new(5, 5);
        room.set_locked(true);
        let (tx, _rx) = mpsc::unbounded_channel();
        let (media, _m) = mpsc::channel(1);
        assert_eq!(room.join_member(1, minfo("x"), tx, media), Err("locked"));
        let (tx, _rx) = mpsc::unbounded_channel();
        assert!(room.add_presenter(2, info("c"), Link::Remote(tx), false).is_err());
    }

    #[test]
    fn relay_rewrites_prefix_and_skips_until_keyframe_when_full() {
        let room = Room::new(5, 10);
        let (pub_a, mut a_rx) = presenter(&room, "a");
        let (m1, _m1_rx, mut media_rx) = member(&room);
        room.watch(m1, 4, pub_a, Layer::Low, true).unwrap();
        let open = drain(&mut a_rx);
        let open = of_type(&open, "open");
        assert_eq!(open[0]["mode"], "relay");
        let session = open[0]["session"].as_u64().unwrap();
        let frame = |kind: u8| {
            let mut f = session.to_be_bytes().to_vec();
            f.push(kind);
            f.extend_from_slice(&[0; 8]);
            f.extend_from_slice(b"data");
            f
        };
        room.presenter_media(pub_a, &frame(1));
        let got = media_rx.try_recv().unwrap();
        assert_eq!(&got[..4], &4u32.to_be_bytes());
        assert_eq!(got[4], 1);
        assert_eq!(&got[13..], b"data");

        // Fila (capacidade 2) enche: o terceiro quadro cai e um quadro-chave é pedido.
        room.presenter_media(pub_a, &frame(2));
        room.presenter_media(pub_a, &frame(2));
        room.presenter_media(pub_a, &frame(2));
        assert_eq!(of_type(&drain(&mut a_rx), "keyframe").len(), 1);
        media_rx.try_recv().unwrap();
        media_rx.try_recv().unwrap();
        // Até o quadro-chave, quadros comuns são descartados.
        room.presenter_media(pub_a, &frame(2));
        assert!(media_rx.try_recv().is_err());
        room.presenter_media(pub_a, &frame(1));
        assert_eq!(media_rx.try_recv().unwrap()[4], 1);

        // Quadro de uma sessão de outro apresentador é ignorado.
        let (pub_b, _b_rx) = presenter(&room, "b");
        room.presenter_media(pub_b, &frame(1));
        assert!(media_rx.try_recv().is_err());
    }
}
