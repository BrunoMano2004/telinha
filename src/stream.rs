//! Lado WebRTC: um único pipeline codifica a tela (e o áudio, se ligado) uma
//! vez e cada espectador recebe um webrtcbin com um `queue` pendurado em cada
//! `tee` (vídeo e áudio).

use anyhow::{Context, Result, bail};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_sdp as gst_sdp;
use gstreamer_video as gst_video;
use gstreamer_webrtc as gst_webrtc;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Signal {
    Offer { sdp: String },
    Answer { sdp: String },
    Ice {
        candidate: String,
        #[serde(rename = "sdpMLineIndex")]
        sdp_mline_index: u32,
    },
    /// Diagnóstico enviado pela página; só é impresso.
    Log { text: String },
}

struct Viewer {
    bin: gst::Bin,
    webrtc: gst::Element,
    /// Um pad de cada tee (vídeo, áudio) ligado a este espectador.
    tee_pads: Vec<(gst::Element, gst::Pad)>,
}

#[derive(Default)]
struct State {
    viewers: HashMap<u64, Viewer>,
    running: bool,
    /// Incrementado a cada entrada/saída; um desligamento agendado só vale se
    /// ninguém entrou desde então.
    generation: u64,
}

pub struct Streamer {
    pub pipeline: gst::Pipeline,
    /// `tee` do vídeo e, com áudio ligado, `atee`.
    tees: Vec<gst::Element>,
    valves: Vec<gst::Element>,
    pub encoder: gst::Element,
    /// Servidor STUN para quem está fora da tailnet descobrir nosso IP público.
    stun: Option<String>,
    state: Mutex<State>,
    teardown: Arc<Mutex<Vec<std::thread::JoinHandle<()>>>>,
}

impl Streamer {
    pub fn new(description: &str, stun: Option<String>) -> Result<Self> {
        let pipeline = gst::parse::launch(description)
            .context("falha ao montar o pipeline")?
            .downcast::<gst::Pipeline>()
            .unwrap();
        let get = |name: &str| pipeline.by_name(name).expect("elemento nomeado no pipeline");
        let audio = pipeline.by_name("atee").is_some();
        let tees: Vec<_> = if audio { vec![get("tee"), get("atee")] } else { vec![get("tee")] };
        let valves: Vec<_> = if audio { vec![get("valve"), get("avalve")] } else { vec![get("valve")] };

        // Cada webrtcbin novo manda RECONFIGURE rio acima. Se isso chega ao
        // pipewiresrc, ele renegocia o formato com o portal em loop e a captura
        // trava. O formato do encoder nunca muda por causa de um espectador.
        for valve in &valves {
            valve.static_pad("sink").unwrap().add_probe(gst::PadProbeType::EVENT_UPSTREAM, |_, info| {
                match info.event() {
                    Some(e) if e.type_() == gst::EventType::Reconfigure => gst::PadProbeReturn::Drop,
                    _ => gst::PadProbeReturn::Ok,
                }
            });
        }

        Ok(Self {
            tees,
            valves,
            encoder: get("enc"),
            stun,
            state: Mutex::new(State::default()),
            teardown: Arc::new(Mutex::new(Vec::new())),
            pipeline,
        })
    }

    pub fn viewer_count(&self) -> usize {
        self.state.lock().unwrap().viewers.len()
    }

    /// Aguarda a remoção dos ramos antes de encerrar o processo.
    pub fn shutdown(&self) {
        let _ = self.pipeline.set_state(gst::State::Null);
        let workers: Vec<_> = self.teardown.lock().unwrap().drain(..).collect();
        for worker in workers { let _ = worker.join(); }
    }

    /// Liga captura + encoder. Retorna a geração atual, usada por `stop_if_idle`.
    pub fn start(&self) -> Result<u64> {
        let mut state = self.state.lock().unwrap();
        state.generation += 1;
        if !state.running {
            self.pipeline.set_state(gst::State::Playing).context("falha ao iniciar o pipeline")?;
            state.running = true;
        }
        Ok(state.generation)
    }

    /// Para captura e encoder se ninguém entrou desde `generation`.
    pub fn stop_if_idle(&self, generation: u64) -> bool {
        let mut state = self.state.lock().unwrap();
        if !state.running || !state.viewers.is_empty() || state.generation != generation {
            return false;
        }
        let _ = self.pipeline.set_state(gst::State::Null);
        state.running = false;
        // Devolve ao sistema a memória que o glibc mantém reservada após
        // desmontar os ramos WebRTC.
        #[cfg(target_os = "linux")]
        unsafe {
            libc::malloc_trim(0);
        }
        true
    }

    pub fn add_viewer(&self, id: u64, tx: UnboundedSender<String>) -> Result<()> {
        self.start()?;

        let bin = gst::Bin::with_name(&format!("viewer-{id}"));
        let webrtc = gst::ElementFactory::make("webrtcbin")
            .property_from_str("bundle-policy", "max-bundle")
            .build()?;
        if let Some(stun) = &self.stun {
            webrtc.set_property("stun-server", stun);
        }
        bin.add(&webrtc)?;

        let mut ghosts = Vec::new();
        for (i, tee) in self.tees.iter().enumerate() {
            let queue = gst::ElementFactory::make("queue")
                .property_from_str("leaky", "downstream")
                .property("max-size-buffers", 0u32)
                .property("max-size-bytes", 0u32)
                .property("max-size-time", 200 * gst::ClockTime::MSECOND)
                .build()?;
            bin.add(&queue)?;
            let sink = webrtc.request_pad_simple("sink_%u").context("webrtcbin sem pad")?;
            queue.static_pad("src").unwrap().link(&sink)?;
            // Mesmo msid nas duas trilhas: o navegador as trata como um stream só
            // e sincroniza áudio com vídeo.
            if sink.find_property("msid").is_some() {
                sink.set_property("msid", "telinha");
            }
            let transceiver = sink.property::<gst_webrtc::WebRTCRTPTransceiver>("transceiver");
            transceiver.set_property("direction", gst_webrtc::WebRTCRTPTransceiverDirection::Sendonly);
            transceiver.set_property("do-nack", i == 0);
            let ghost = gst::GhostPad::builder_with_target(&queue.static_pad("sink").unwrap())?
                .name(format!("sink_{i}"))
                .build();
            bin.add_pad(&ghost)?;
            ghosts.push((tee.clone(), ghost));
        }

        let offer_tx = tx.clone();
        webrtc.connect("on-negotiation-needed", false, move |values| {
            let webrtc = values[0].get::<gst::Element>().unwrap();
            let tx = offer_tx.clone();
            let w = webrtc.clone();
            let promise = gst::Promise::with_change_func(move |reply| {
                let offer = match reply {
                    Ok(Some(s)) => s.get::<gst_webrtc::WebRTCSessionDescription>("offer"),
                    _ => return eprintln!("falha ao criar oferta SDP"),
                };
                let Ok(offer) = offer else { return };
                w.emit_by_name::<()>("set-local-description", &[&offer, &None::<gst::Promise>]);
                let sdp = offer.sdp().as_text().unwrap_or_default();
                let _ = tx.send(serde_json::to_string(&Signal::Offer { sdp }).unwrap());
            });
            webrtc.emit_by_name::<()>("create-offer", &[&None::<gst::Structure>, &promise]);
            None
        });

        webrtc.connect("on-ice-candidate", false, move |values| {
            let sdp_mline_index = values[1].get::<u32>().unwrap();
            let candidate = values[2].get::<String>().unwrap();
            let _ = tx.send(serde_json::to_string(&Signal::Ice { candidate, sdp_mline_index }).unwrap());
            None
        });

        // O ramo precisa estar em PLAYING antes de ser ligado ao tee; senão o
        // primeiro push devolve FLUSHING e a fonte de vídeo para de vez.
        self.pipeline.add(&bin)?;
        bin.sync_state_with_parent()?;
        let mut tee_pads = Vec::new();
        for (tee, ghost) in ghosts {
            let tee_pad = tee.request_pad_simple("src_%u").unwrap();
            tee_pad.link(&ghost)?;
            tee_pads.push((tee, tee_pad));
        }

        let mut state = self.state.lock().unwrap();
        state.viewers.insert(id, Viewer { bin, webrtc, tee_pads });
        for valve in &self.valves {
            valve.set_property("drop", false);
        }
        drop(state);

        self.request_keyframe();
        Ok(())
    }

    pub fn handle_message(&self, id: u64, text: &str) -> Result<()> {
        let webrtc = match self.state.lock().unwrap().viewers.get(&id) {
            Some(v) => v.webrtc.clone(),
            None => bail!("espectador desconhecido"),
        };
        match serde_json::from_str::<Signal>(text)? {
            Signal::Answer { sdp } => {
                let sdp = gst_sdp::SDPMessage::parse_buffer(sdp.as_bytes())?;
                let answer = gst_webrtc::WebRTCSessionDescription::new(gst_webrtc::WebRTCSDPType::Answer, sdp);
                webrtc.emit_by_name::<()>("set-remote-description", &[&answer, &None::<gst::Promise>]);
            }
            Signal::Ice { candidate, sdp_mline_index } => {
                // Candidatos mDNS (.local) do navegador não resolvem aqui; o ICE
                // ainda conecta porque o navegador alcança nosso IP do Tailscale
                // (ou, pelo túnel, nosso IP público descoberto via STUN).
                if !candidate.is_empty() && !candidate.contains(".local") {
                    webrtc.emit_by_name::<()>("add-ice-candidate", &[&sdp_mline_index, &candidate]);
                }
            }
            Signal::Log { text } => println!("  #{id} {text}"),
            Signal::Offer { .. } => bail!("o servidor é quem envia a oferta"),
        }
        Ok(())
    }

    /// Remove o espectador. Se foi o último, devolve a geração para agendar
    /// `stop_if_idle`.
    pub fn remove_viewer(&self, id: u64) -> Option<u64> {
        let mut state = self.state.lock().unwrap();
        let viewer = state.viewers.remove(&id)?;
        state.generation += 1;
        let idle = state.viewers.is_empty().then_some(state.generation);
        if idle.is_some() {
            for valve in &self.valves {
                valve.set_property("drop", true);
            }
        }
        drop(state);

        let Viewer { bin, tee_pads, .. } = viewer;
        bin.set_locked_state(true);
        // Desconecta cada ramo só quando o tee não estiver empurrando dados nele;
        // o bin sai do pipeline depois que o último pad for desligado.
        let remaining = Arc::new(AtomicUsize::new(tee_pads.len()));
        for (tee, tee_pad) in tee_pads {
            let (bin, pipeline, remaining) = (bin.clone(), self.pipeline.clone(), remaining.clone());
            let teardown = self.teardown.clone();
            tee_pad.add_probe(gst::PadProbeType::IDLE, move |pad, _| {
                if let Some(peer) = pad.peer() {
                    let _ = pad.unlink(&peer);
                }
                let last = remaining.fetch_sub(1, Ordering::SeqCst) == 1;
                let (pad, tee, bin, pipeline) = (pad.clone(), tee.clone(), bin.clone(), pipeline.clone());
                let worker = std::thread::spawn(move || {
                    tee.release_request_pad(&pad);
                    if last {
                        let _ = bin.set_state(gst::State::Null);
                        let _ = pipeline.remove(&bin);
                    }
                });
                teardown.lock().unwrap().push(worker);
                gst::PadProbeReturn::Remove
            });
        }
        idle
    }

    /// Pede um keyframe (IDR) ao encoder — usado quando alguém entra.
    pub fn request_keyframe(&self) {
        let event = gst_video::UpstreamForceKeyUnitEvent::builder().all_headers(true).build();
        self.encoder.static_pad("src").unwrap().send_event(event);
    }
}
