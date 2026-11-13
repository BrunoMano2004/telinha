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
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
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
    /// Outras mensagens da página (ex.: um "hello" repetido) são ignoradas.
    #[serde(other)]
    Other,
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
    /// Limite de quadros por segundo antes do encoder, ajustável ao vivo.
    max_fps: Arc<AtomicU32>,
}

impl Streamer {
    pub fn new(description: &str, stun: Option<String>, fps: u32) -> Result<Self> {
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

        // Limite de fps por timestamp: a captura roda no máximo dela e aqui só
        // descartamos quadros. Mudar o limite não renegocia formato nenhum
        // (renegociar com o PipeWire já travou a captura antes).
        let max_fps = Arc::new(AtomicU32::new(fps.max(1)));
        let limit = max_fps.clone();
        let last = Mutex::new(None::<gst::ClockTime>);
        valves[0].static_pad("sink").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            let Some(pts) = info.buffer().and_then(|b| b.pts()) else { return gst::PadProbeReturn::Ok };
            // 10% de folga para o jitter de quem entrega a ~60 Hz.
            let min_gap = gst::ClockTime::SECOND.nseconds() * 9 / 10 / u64::from(limit.load(Ordering::Relaxed).max(1));
            let mut last = last.lock().unwrap();
            // Timestamp voltando = captura reiniciada ou trocada: recomeça a conta.
            if let Some(prev) = *last
                && pts > prev
                && (pts - prev).nseconds() < min_gap
            {
                return gst::PadProbeReturn::Drop;
            }
            *last = Some(pts);
            gst::PadProbeReturn::Ok
        });

        Ok(Self {
            tees,
            valves,
            encoder: get("enc"),
            stun,
            state: Mutex::new(State::default()),
            teardown: Arc::new(Mutex::new(Vec::new())),
            max_fps,
            pipeline,
        })
    }

    pub fn max_fps(&self) -> u32 {
        self.max_fps.load(Ordering::Relaxed)
    }

    /// Qualidade ao vivo: bitrate no encoder e limite de fps antes dele.
    pub fn set_quality(&self, kbps: u32, fps: u32) {
        self.max_fps.store(fps.max(1), Ordering::Relaxed);
        crate::capture::set_bitrate(&self.encoder, kbps, fps.max(1));
    }

    /// Troca o que está sendo capturado (outra tela/janela/monitor) sem
    /// derrubar quem assiste: só o elemento `screen` reinicia, com as novas
    /// propriedades aplicadas por `apply`.
    pub fn replace_screen(&self, apply: impl FnOnce(&gst::Element)) -> Result<()> {
        let screen = self.pipeline.by_name("screen").context("esta captura não pode ser trocada (modo de teste?)")?;
        let state = self.state.lock().unwrap();
        if state.running {
            screen.set_state(gst::State::Null).context("falha ao parar a captura")?;
            apply(&screen);
            screen.sync_state_with_parent().context("falha ao religar a captura")?;
        } else {
            apply(&screen);
        }
        drop(state);
        self.request_keyframe();
        Ok(())
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
            Signal::Other => {}
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
                let mut workers = teardown.lock().unwrap();
                // Threads já terminadas não precisam esperar o fim do programa.
                workers.retain(|w| !w.is_finished());
                workers.push(worker);
                gst::PadProbeReturn::Remove
            });
        }
        idle
    }

    /// Rede de cada espectador, do vídeo: (ssrc, fração perdida, jitter em s,
    /// pacotes enviados, pedidos de retransmissão), dos relatórios RTCP.
    pub fn viewer_network(&self) -> Vec<(u32, f64, f64, u64, u32)> {
        let mut out = Vec::new();
        for stats in self.viewer_stats() {
            let mut remote = None;
            let mut sent = None;
            for (_, value) in stats.iter() {
                let Ok(s) = value.get::<gst::Structure>() else { continue };
                if s.get::<&str>("kind").ok() != Some("video") {
                    continue;
                }
                match s.get::<gst_webrtc::WebRTCStatsType>("type") {
                    Ok(gst_webrtc::WebRTCStatsType::RemoteInboundRtp) => {
                        remote = Some((s.get::<f64>("fraction-lost").unwrap_or(0.0), s.get::<f64>("jitter").unwrap_or(0.0)))
                    }
                    Ok(gst_webrtc::WebRTCStatsType::OutboundRtp) => {
                        sent = Some((
                            s.get::<u32>("ssrc").unwrap_or(0),
                            s.get::<u64>("packets-sent").unwrap_or(0),
                            s.get::<u32>("nack-count").unwrap_or(0),
                        ))
                    }
                    _ => {}
                }
            }
            if let (Some((loss, jitter)), Some((ssrc, packets, nacks))) = (remote, sent) {
                out.push((ssrc, loss, jitter, packets, nacks));
            }
        }
        out
    }

    /// Estatísticas WebRTC de cada espectador (resposta do `get-stats`).
    pub fn viewer_stats(&self) -> Vec<gst::Structure> {
        let webrtcs: Vec<gst::Element> = self.state.lock().unwrap().viewers.values().map(|v| v.webrtc.clone()).collect();
        webrtcs
            .iter()
            .filter_map(|webrtc| {
                let promise = gst::Promise::new();
                webrtc.emit_by_name::<()>("get-stats", &[&None::<gst::Pad>, &promise]);
                match promise.wait() {
                    gst::PromiseResult::Replied => promise.get_reply().map(|r| r.to_owned()),
                    _ => None,
                }
            })
            .collect()
    }

    /// Pede um keyframe (IDR) ao encoder — usado quando alguém entra.
    pub fn request_keyframe(&self) {
        let event = gst_video::UpstreamForceKeyUnitEvent::builder().all_headers(true).build();
        self.encoder.static_pad("src").unwrap().send_event(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fonte a 60 fps e limite em 30: passam ~30 quadros por segundo, e
    /// subir o limite ao vivo volta a deixar passar tudo.
    #[test]
    fn fps_limit_drops_frames_live() {
        gst::init().unwrap();
        let streamer = Streamer::new(
            "videotestsrc is-live=true ! video/x-raw,width=64,height=48,framerate=60/1 \
             ! valve name=valve drop=false ! identity name=enc ! tee name=tee ! fakesink name=out sync=false",
            None,
            30,
        )
        .unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        streamer.pipeline.by_name("out").unwrap().static_pad("sink").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            c.fetch_add(1, Ordering::Relaxed);
            gst::PadProbeReturn::Ok
        });
        let measure = || {
            count.store(0, Ordering::Relaxed);
            std::thread::sleep(std::time::Duration::from_millis(1000));
            count.load(Ordering::Relaxed)
        };
        streamer.pipeline.set_state(gst::State::Playing).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let at30 = measure();
        streamer.set_quality(1000, 60);
        let at60 = measure();
        streamer.pipeline.set_state(gst::State::Null).unwrap();
        assert!((26..=34).contains(&at30), "limite 30 fps deixou passar {at30}");
        assert!((54..=66).contains(&at60), "limite 60 fps deixou passar {at60}");
    }

    /// Trocar a captura com o pipeline rodando: o elemento `screen` reinicia
    /// com as novas propriedades e os quadros continuam chegando.
    #[test]
    fn replace_screen_keeps_frames_flowing() {
        gst::init().unwrap();
        let streamer = Streamer::new(
            "videotestsrc name=screen is-live=true pattern=snow ! video/x-raw,width=64,height=48,framerate=30/1 \
             ! valve name=valve drop=false ! identity name=enc ! tee name=tee ! fakesink name=out sync=false",
            None,
            60,
        )
        .unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        streamer.pipeline.by_name("out").unwrap().static_pad("sink").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            c.fetch_add(1, Ordering::Relaxed);
            gst::PadProbeReturn::Ok
        });
        streamer.start().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));
        streamer.replace_screen(|s| s.set_property_from_str("pattern", "ball")).unwrap();
        count.store(0, Ordering::Relaxed);
        std::thread::sleep(std::time::Duration::from_millis(1000));
        let after = count.load(Ordering::Relaxed);
        let pattern = streamer.pipeline.by_name("screen").unwrap().property_value("pattern");
        streamer.pipeline.set_state(gst::State::Null).unwrap();
        assert!(after >= 20, "depois da troca passaram só {after} quadros");
        assert_eq!(format!("{pattern:?}").contains("ball") || format!("{pattern:?}").contains("Ball"), true);
    }
}
