//! Quem compartilha a tela numa jam: sessões de espectador que chegam pela
//! sala (e não por um WebSocket próprio) e viram ramos do `Streamer`, como no
//! modo normal. Serve tanto à tela do anfitrião (sessões abertas direto pela
//! sala) quanto à de quem entrou com `--join` (sessões pelo uplink).

use crate::stream::{Layer, Streamer};
use anyhow::Result;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// Recebe cada mensagem de sinalização (JSON do `Signal`) de uma sessão.
pub type SignalOut = Box<dyn Fn(String) + Send + Sync>;

pub struct Publisher {
    pub streamer: Arc<Streamer>,
    sessions: Mutex<HashSet<u64>>,
}

impl Publisher {
    pub fn new(streamer: Arc<Streamer>) -> Self {
        Self { streamer, sessions: Mutex::new(HashSet::new()) }
    }

    /// Abre a sessão `session`: WebRTC (sinalização por `signal`) ou relé
    /// (quadros por `media`, cada um começando por `prefix`).
    pub fn open(
        &self,
        session: u64,
        layer: Layer,
        relay: bool,
        signal: SignalOut,
        media: mpsc::Sender<Vec<u8>>,
        prefix: Vec<u8>,
    ) -> Result<()> {
        if !self.sessions.lock().unwrap().insert(session) {
            anyhow::bail!("sessão {session} já existe");
        }
        let result = if relay {
            self.streamer.add_tunnel_viewer(session, media, layer, prefix)
        } else {
            let (tx, mut rx) = mpsc::unbounded_channel::<String>();
            // O Streamer manda a sinalização por um canal; aqui ela ganha o
            // envelope da sessão. Termina quando o ramo é desmontado.
            tokio::spawn(async move {
                while let Some(text) = rx.recv().await {
                    signal(text);
                }
            });
            self.streamer.add_viewer(session, tx, layer)
        };
        if result.is_err() {
            self.sessions.lock().unwrap().remove(&session);
        }
        result
    }

    /// Resposta SDP, candidatos ICE e diagnósticos do navegador.
    pub fn signal(&self, session: u64, payload: &str) -> Result<()> {
        if !self.sessions.lock().unwrap().contains(&session) {
            anyhow::bail!("sessão {session} desconhecida");
        }
        self.streamer.handle_message(session, payload)
    }

    /// Pedido de quadro-chave (relé, depois de perder quadros).
    pub fn keyframe(&self, session: u64) {
        if self.sessions.lock().unwrap().contains(&session) {
            self.streamer.request_keyframe_of(session);
        }
    }

    pub fn close(&self, session: u64) {
        if !self.sessions.lock().unwrap().remove(&session) {
            return;
        }
        if let Some(generation) = self.streamer.remove_viewer(session) {
            crate::stream::schedule_idle_stop(self.streamer.clone(), generation);
        }
    }

    /// Fecha tudo (compartilhamento pausado ou conexão com a sala perdida).
    pub fn close_all(&self) {
        let sessions: Vec<u64> = self.sessions.lock().unwrap().iter().copied().collect();
        for session in sessions {
            self.close(session);
        }
    }
}
