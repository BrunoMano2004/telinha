//! Quem está assistindo: identificação, expulsão e bloqueio. As interfaces
//! (card do Caelestia, app do Windows) leem os eventos `@viewers` e `@blocked`
//! da saída padrão e mandam `kick <id>`, `block <id>` e `unblock <chave>`.
//!
//! O navegador não revela o nome do computador, então quem assiste digita um
//! nome na página; junto vai o aparelho (navegador · sistema) e um id aleatório
//! guardado no navegador. O bloqueio vale para esse id e para o IP.

use serde::Serialize;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::oneshot;

/// O que a página informa ao entrar.
pub struct Hello {
    pub name: String,
    pub device: String,
    pub client: String,
}

impl Hello {
    /// Lê `{"type":"hello",...}`; textos limpos e com tamanho limitado.
    pub fn parse(text: &str) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_str(text).ok()?;
        if v["type"] != "hello" {
            return None;
        }
        let field = |key: &str, max: usize| -> String {
            let raw = v[key].as_str().unwrap_or_default();
            raw.chars().filter(|c| !c.is_control()).take(max).collect::<String>().trim().to_string()
        };
        let client: String = field("client", 64).chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
        let name = field("name", 40);
        Some(Self {
            name: if name.is_empty() { "Anônimo".into() } else { name },
            device: field("device", 60),
            client,
        })
    }
}

#[derive(Clone, Serialize)]
struct ViewerInfo {
    id: u64,
    name: String,
    device: String,
    ip: String,
    /// Entrada, em segundos desde 1970 (as interfaces mostram "há X min").
    since: u64,
}

#[derive(Clone, Serialize)]
struct Blocked {
    key: String,
    name: String,
    device: String,
    ip: String,
    #[serde(skip)]
    client: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Removal {
    Kicked,
    Blocked,
}

struct Entry {
    info: ViewerInfo,
    client: String,
    remove: Option<oneshot::Sender<Removal>>,
}

#[derive(Default)]
pub struct Registry {
    viewers: Mutex<HashMap<u64, Entry>>,
    blocked: Mutex<Vec<Blocked>>,
    next_block: AtomicU64,
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// IP que identifica alguém. Vazio quando não serve (o 127.0.0.1 do túnel
/// seria o mesmo para todo mundo).
pub fn useful_ip(ip: &str) -> String {
    match ip.parse::<std::net::IpAddr>() {
        Ok(addr) if !addr.is_loopback() && !addr.is_unspecified() => ip.to_string(),
        _ => String::new(),
    }
}

impl Registry {
    fn matches(b: &Blocked, client: &str, ip: &str) -> bool {
        (!b.client.is_empty() && b.client == client) || (!b.ip.is_empty() && b.ip == ip)
    }

    pub fn is_blocked(&self, client: &str, ip: &str) -> bool {
        self.blocked.lock().unwrap().iter().any(|b| Self::matches(b, client, ip))
    }

    /// Registra quem entrou. O receptor avisa se a pessoa foi expulsa ou bloqueada.
    pub fn add(&self, id: u64, hello: Hello, ip: String) -> oneshot::Receiver<Removal> {
        let (tx, rx) = oneshot::channel();
        let info = ViewerInfo { id, name: hello.name, device: hello.device, ip, since: now() };
        self.viewers.lock().unwrap().insert(id, Entry { info, client: hello.client, remove: Some(tx) });
        self.emit_viewers();
        rx
    }

    pub fn remove(&self, id: u64) {
        if self.viewers.lock().unwrap().remove(&id).is_some() {
            self.emit_viewers();
        }
    }

    pub fn name(&self, id: u64) -> Option<String> {
        self.viewers.lock().unwrap().get(&id).map(|e| e.info.name.clone())
    }

    fn signal(&self, id: u64, why: Removal) -> bool {
        let tx = self.viewers.lock().unwrap().get_mut(&id).and_then(|e| e.remove.take());
        tx.is_some_and(|tx| tx.send(why).is_ok())
    }

    /// Desconecta; a pessoa pode entrar de novo com o link.
    pub fn kick(&self, id: u64) -> bool {
        self.signal(id, Removal::Kicked)
    }

    /// Desconecta e impede de voltar (mesmo navegador ou mesmo IP), inclusive
    /// outras abas/aparelhos dessa pessoa que estejam assistindo.
    pub fn block(&self, id: u64) -> bool {
        let Some((info, client)) = self.viewers.lock().unwrap().get(&id).map(|e| (e.info.clone(), e.client.clone())) else {
            return false;
        };
        let entry = Blocked {
            key: self.next_block.fetch_add(1, Ordering::Relaxed).to_string(),
            name: info.name,
            device: info.device,
            ip: info.ip,
            client,
        };
        let same: Vec<u64> = self
            .viewers
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, e)| Self::matches(&entry, &e.client, &e.info.ip))
            .map(|(&id, _)| id)
            .collect();
        self.blocked.lock().unwrap().push(entry);
        self.emit_blocked();
        for id in same {
            self.signal(id, Removal::Blocked);
        }
        true
    }

    pub fn unblock(&self, key: &str) -> bool {
        let mut blocked = self.blocked.lock().unwrap();
        let before = blocked.len();
        blocked.retain(|b| b.key != key);
        let changed = blocked.len() != before;
        drop(blocked);
        if changed {
            self.emit_blocked();
        }
        changed
    }

    pub fn emit_viewers(&self) {
        let mut list: Vec<ViewerInfo> = self.viewers.lock().unwrap().values().map(|e| e.info.clone()).collect();
        list.sort_by_key(|v| v.id);
        println!("@viewers {}", serde_json::to_string(&list).unwrap_or_default());
    }

    pub fn emit_blocked(&self) {
        let list = self.blocked.lock().unwrap().clone();
        println!("@blocked {}", serde_json::to_string(&list).unwrap_or_default());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello(client: &str) -> Hello {
        Hello { name: "Ana".into(), device: "Firefox · Linux".into(), client: client.into() }
    }

    #[test]
    fn hello_is_sanitized() {
        let h = Hello::parse(r#"{"type":"hello","name":"  A\u0007na  ","device":"x","client":"ab$c-1"}"#).unwrap();
        assert_eq!((h.name.as_str(), h.client.as_str()), ("Ana", "abc-1"));
        assert_eq!(Hello::parse(r#"{"type":"hello"}"#).unwrap().name, "Anônimo");
        assert!(Hello::parse(r#"{"type":"answer"}"#).is_none());
    }

    #[test]
    fn block_matches_browser_or_ip_and_kicks_both() {
        let r = Registry::default();
        let mut a = r.add(1, hello("c1"), "1.2.3.4".into());
        let mut b = r.add(2, hello("c2"), "1.2.3.4".into());
        let mut c = r.add(3, hello("c3"), "5.6.7.8".into());
        assert!(r.block(1));
        assert_eq!(a.try_recv().unwrap(), Removal::Blocked);
        assert_eq!(b.try_recv().unwrap(), Removal::Blocked);
        assert!(c.try_recv().is_err());
        assert!(r.is_blocked("c1", ""));
        assert!(r.is_blocked("outro", "1.2.3.4"));
        assert!(!r.is_blocked("c3", "5.6.7.8"));
        assert!(r.unblock("0"));
        assert!(!r.is_blocked("c1", "1.2.3.4"));
    }

    #[test]
    fn empty_ids_never_match() {
        let r = Registry::default();
        let _rx = r.add(1, hello(""), String::new());
        r.block(1);
        assert!(!r.is_blocked("", ""));
        assert!(!r.is_blocked("x", "9.9.9.9"));
        assert_eq!(useful_ip("127.0.0.1"), "");
        assert_eq!(useful_ip("::1"), "");
        assert_eq!(useful_ip("8.8.8.8"), "8.8.8.8");
    }
}
