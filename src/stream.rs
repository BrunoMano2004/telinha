//! Lado WebRTC: um único pipeline codifica a tela (e o áudio, se ligado) uma
//! vez e cada espectador recebe um webrtcbin com um `queue` pendurado em cada
//! `tee` (vídeo e áudio).
//!
//! Na jam a tela sai em duas camadas: a alta (`tee`, com áudio) para quem
//! selecionou esta tela e a miniatura (`ltee`, só vídeo) para a grade. Cada
//! camada tem sua `valve`, aberta só enquanto alguém assiste àquela camada.

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
    /// Modo túnel: a página precisa de um quadro-chave para (re)começar.
    Keyframe,
    /// Outras mensagens da página (ex.: um "hello" repetido) são ignoradas.
    #[serde(other)]
    Other,
}

/// Camada de vídeo que um espectador recebe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Layer {
    /// Qualidade cheia (a de sempre), com o som junto.
    High,
    /// Miniatura da grade da jam: ~640x360, poucos quadros e sem som.
    Low,
}

/// Quadros por segundo da miniatura.
pub const LOW_FPS: u32 = 15;
/// Bitrate da miniatura (kbps).
pub const LOW_KBPS: u32 = 800;

/// Uma camada de vídeo do pipeline: portão, encoder e tee.
struct VideoLayer {
    tee: gst::Element,
    valve: gst::Element,
    encoder: gst::Element,
}

struct Viewer {
    bin: gst::Bin,
    /// `None` no modo túnel (vídeo e áudio pelo WebSocket, sem WebRTC).
    webrtc: Option<gst::Element>,
    /// Um pad de cada tee (vídeo, áudio) ligado a este espectador.
    tee_pads: Vec<(gst::Element, gst::Pad)>,
    layer: Layer,
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
    /// Camada alta (`tee`/`valve`/`enc`).
    high: VideoLayer,
    /// Miniatura (`ltee`/`lvalve`/`lenc`), só no modo jam.
    low: Option<VideoLayer>,
    /// `atee` e `avalve`, com áudio ligado.
    audio: Option<(gst::Element, gst::Element)>,
    pub encoder: gst::Element,
    /// Servidor STUN para quem está fora da tailnet descobrir nosso IP público.
    stun: Option<String>,
    state: Mutex<State>,
    teardown: Arc<Mutex<Vec<std::thread::JoinHandle<()>>>>,
    /// Limite de quadros por segundo antes do encoder, ajustável ao vivo.
    max_fps: Arc<AtomicU32>,
    /// Quadros de vídeo enviados/descartados no modo túnel.
    tunnel_sent: Arc<std::sync::atomic::AtomicU64>,
    tunnel_dropped: Arc<std::sync::atomic::AtomicU64>,
}

/// Sem ninguém assistindo, espera isto antes de parar captura e encoder.
pub const IDLE_STOP_DELAY: std::time::Duration = std::time::Duration::from_secs(10);

/// Espera um pouco (cobre recarregar a página) e, se continuar sem ninguém,
/// para captura e encoder — o processo volta a ~30 MB e ~0% de CPU.
pub fn schedule_idle_stop(streamer: Arc<Streamer>, generation: u64) {
    tokio::spawn(async move {
        tokio::time::sleep(IDLE_STOP_DELAY).await;
        if streamer.stop_if_idle(generation) {
            println!("ninguém assistindo · captura e encoder pausados");
        }
    });
}

/// Limite de fps por timestamp: a captura roda no máximo dela e aqui só
/// descartamos quadros. Mudar o limite não renegocia formato nenhum
/// (renegociar com o PipeWire já travou a captura antes).
fn limit_fps(valve: &gst::Element, limit: Arc<AtomicU32>) {
    let last = Mutex::new(None::<gst::ClockTime>);
    valve.static_pad("sink").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, info| {
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
}

impl Streamer {
    pub fn new(description: &str, stun: Option<String>, fps: u32) -> Result<Self> {
        let pipeline = gst::parse::launch(description)
            .context("falha ao montar o pipeline")?
            .downcast::<gst::Pipeline>()
            .unwrap();
        let get = |name: &str| pipeline.by_name(name).expect("elemento nomeado no pipeline");
        let high = VideoLayer { tee: get("tee"), valve: get("valve"), encoder: get("enc") };
        let low = pipeline
            .by_name("ltee")
            .map(|tee| VideoLayer { tee, valve: get("lvalve"), encoder: get("lenc") });
        let audio = pipeline.by_name("atee").map(|tee| (tee, get("avalve")));
        let mut valves = vec![high.valve.clone()];
        valves.extend(low.iter().map(|l| l.valve.clone()));
        valves.extend(audio.iter().map(|(_, v)| v.clone()));

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

        let max_fps = Arc::new(AtomicU32::new(fps.max(1)));
        limit_fps(&high.valve, max_fps.clone());
        if let Some(low) = &low {
            limit_fps(&low.valve, Arc::new(AtomicU32::new(LOW_FPS)));
        }

        Ok(Self {
            encoder: high.encoder.clone(),
            high,
            low,
            audio,
            stun,
            state: Mutex::new(State::default()),
            teardown: Arc::new(Mutex::new(Vec::new())),
            max_fps,
            tunnel_sent: Arc::default(),
            tunnel_dropped: Arc::default(),
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

    /// Camada que este pipeline realmente tem: sem miniatura (fora da jam),
    /// todo mundo recebe a alta.
    fn layer(&self, layer: Layer) -> Layer {
        if self.low.is_some() { layer } else { Layer::High }
    }

    fn video(&self, layer: Layer) -> &VideoLayer {
        match layer {
            Layer::Low => self.low.as_ref().unwrap_or(&self.high),
            Layer::High => &self.high,
        }
    }

    /// Tees que alimentam um espectador da camada: o vídeo dela e, na alta, o
    /// áudio (`true` = vídeo).
    fn branch_tees(&self, layer: Layer) -> Vec<(gst::Element, bool)> {
        let mut tees = vec![(self.video(layer).tee.clone(), true)];
        if layer == Layer::High
            && let Some((atee, _)) = &self.audio
        {
            tees.push((atee.clone(), false));
        }
        tees
    }

    /// Cada portão só fica aberto se alguém assiste àquela camada; o áudio
    /// acompanha a camada alta.
    fn update_valves(&self, state: &State) {
        let watching = |layer| state.viewers.values().any(|v| v.layer == layer);
        let high = watching(Layer::High);
        self.high.valve.set_property("drop", !high);
        if let Some(low) = &self.low {
            low.valve.set_property("drop", !watching(Layer::Low));
        }
        if let Some((_, avalve)) = &self.audio {
            avalve.set_property("drop", !high);
        }
    }

    /// Aguarda a remoção dos ramos antes de encerrar o processo (até 3 s: um
    /// ramo que não desmonta não pode prender a saída do programa).
    pub fn shutdown(&self) {
        let _ = self.pipeline.set_state(gst::State::Null);
        let workers: Vec<_> = self.teardown.lock().unwrap().drain(..).collect();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        for worker in workers {
            while !worker.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            if worker.is_finished() {
                let _ = worker.join();
            }
        }
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

    pub fn add_viewer(&self, id: u64, tx: UnboundedSender<String>, layer: Layer) -> Result<()> {
        let layer = self.layer(layer);
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
        for (i, (tee, video)) in self.branch_tees(layer).into_iter().enumerate() {
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
            transceiver.set_property("do-nack", video);
            let ghost = gst::GhostPad::builder_with_target(&queue.static_pad("sink").unwrap())?
                .name(format!("sink_{i}"))
                .build();
            bin.add_pad(&ghost)?;
            ghosts.push((tee, ghost));
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
        state.viewers.insert(id, Viewer { bin, webrtc: Some(webrtc), tee_pads, layer });
        self.update_valves(&state);
        drop(state);

        self.request_keyframe_for(layer);
        Ok(())
    }

    /// Espectador pelo túnel: quando a conexão direta (WebRTC) não fecha, o
    /// vídeo e o áudio já codificados vão pelo próprio WebSocket, que passa pela
    /// Cloudflare. Cada mensagem binária é [tipo][pts em µs, u64][dados]:
    /// tipo 1 = quadro-chave H.264, 2 = quadro H.264, 3 = pacote Opus.
    /// Se a conexão da pessoa não der conta (fila cheia), descarta até o
    /// próximo quadro-chave e pede um novo: a imagem pula, mas não corrompe.
    /// `prefix` vai antes de cada mensagem (na jam, a sessão a que ela pertence).
    pub fn add_tunnel_viewer(
        &self,
        id: u64,
        tx: tokio::sync::mpsc::Sender<Vec<u8>>,
        layer: Layer,
        prefix: Vec<u8>,
    ) -> Result<()> {
        let layer = self.layer(layer);
        self.start()?;
        let bin = gst::Bin::with_name(&format!("tunnel-{id}"));
        let mut ghosts = Vec::new();
        for (i, (tee, video)) in self.branch_tees(layer).into_iter().enumerate() {
            let description = if video {
                "queue leaky=downstream max-size-buffers=0 max-size-bytes=0 max-size-time=200000000 \
                 ! rtph264depay ! h264parse config-interval=-1 \
                 ! video/x-h264,stream-format=byte-stream,alignment=au \
                 ! fakesink name=out signal-handoffs=true sync=false async=false"
            } else {
                "queue leaky=downstream max-size-buffers=0 max-size-bytes=0 max-size-time=200000000 \
                 ! rtpopusdepay ! fakesink name=out signal-handoffs=true sync=false async=false"
            };
            let branch = gst::parse::bin_from_description(description, false)?;
            let out = branch.by_name("out").unwrap();
            out.set_property("name", format!("out{i}"));
            let queue_sink = branch.iterate_sorted().into_iter().flatten().last()
                .and_then(|first| first.static_pad("sink"))
                .context("ramo do túnel sem entrada")?;
            let ghost = gst::GhostPad::builder_with_target(&queue_sink)?.name(format!("sink_{i}")).build();
            branch.add_pad(&ghost)?;
            bin.add(&branch)?;
            let outer = gst::GhostPad::builder_with_target(&ghost)?.name(format!("sink_{i}")).build();
            bin.add_pad(&outer)?;
            ghosts.push((tee, outer));

            let tx = tx.clone();
            let prefix = prefix.clone();
            let need_key = Arc::new(std::sync::atomic::AtomicBool::new(true));
            // Só a camada alta conta para a qualidade automática.
            let (sent, dropped) = if layer == Layer::High {
                (self.tunnel_sent.clone(), self.tunnel_dropped.clone())
            } else {
                (Arc::default(), Arc::default())
            };
            let encoder = self.video(layer).encoder.clone();
            let last_request = Mutex::new(std::time::Instant::now() - std::time::Duration::from_secs(10));
            out.connect("handoff", false, move |values| {
                let Ok(buffer) = values[1].get::<gst::Buffer>() else { return None };
                let key = video && !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);
                let request_key = || {
                    let mut last = last_request.lock().unwrap();
                    if last.elapsed() > std::time::Duration::from_millis(500) {
                        *last = std::time::Instant::now();
                        let event = gst_video::UpstreamForceKeyUnitEvent::builder().all_headers(true).build();
                        encoder.static_pad("src").unwrap().send_event(event);
                    }
                };
                if video && !key && need_key.load(Ordering::Relaxed) {
                    dropped.fetch_add(1, Ordering::Relaxed);
                    request_key();
                    return None;
                }
                let Ok(map) = buffer.map_readable() else { return None };
                let pts = buffer.pts().map(|t| t.useconds()).unwrap_or(0);
                let mut message = Vec::with_capacity(prefix.len() + 9 + map.len());
                message.extend_from_slice(&prefix);
                message.push(if !video { 3 } else if key { 1 } else { 2 });
                message.extend_from_slice(&pts.to_be_bytes());
                message.extend_from_slice(&map);
                match tx.try_send(message) {
                    Ok(()) => {
                        sent.fetch_add(1, Ordering::Relaxed);
                        if key {
                            need_key.store(false, Ordering::Relaxed);
                        }
                    }
                    Err(_) if video => {
                        // Sem espaço: descarta até o próximo quadro-chave.
                        dropped.fetch_add(1, Ordering::Relaxed);
                        need_key.store(true, Ordering::Relaxed);
                        request_key();
                    }
                    Err(_) => {}
                }
                None
            });
        }

        self.pipeline.add(&bin)?;
        bin.sync_state_with_parent()?;
        let mut tee_pads = Vec::new();
        for (tee, ghost) in ghosts {
            let tee_pad = tee.request_pad_simple("src_%u").unwrap();
            tee_pad.link(&ghost)?;
            tee_pads.push((tee, tee_pad));
        }
        let mut state = self.state.lock().unwrap();
        state.viewers.insert(id, Viewer { bin, webrtc: None, tee_pads, layer });
        self.update_valves(&state);
        drop(state);
        self.request_keyframe_for(layer);
        Ok(())
    }

    /// Quadros de vídeo enviados e descartados pelo túnel, desde o início
    /// (a qualidade automática usa a proporção de descartes como "perda").
    pub fn tunnel_counters(&self) -> (u64, u64) {
        (self.tunnel_sent.load(Ordering::Relaxed), self.tunnel_dropped.load(Ordering::Relaxed))
    }

    /// Espectadores pelo túnel na camada alta (a que a qualidade automática ajusta).
    pub fn tunnel_viewer_count(&self) -> usize {
        self.state.lock().unwrap().viewers.values().filter(|v| v.webrtc.is_none() && v.layer == Layer::High).count()
    }

    pub fn handle_message(&self, id: u64, text: &str) -> Result<()> {
        let webrtc = match self.state.lock().unwrap().viewers.get(&id) {
            Some(v) => v.webrtc.clone(),
            None => bail!("espectador desconhecido"),
        };
        let signal = serde_json::from_str::<Signal>(text)?;
        // No modo túnel só os diagnósticos da página interessam.
        let Some(webrtc) = webrtc else {
            match signal {
                Signal::Log { text } => println!("  #{id} {text}"),
                Signal::Keyframe => self.request_keyframe(),
                _ => {}
            }
            return Ok(());
        };
        match signal {
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
            Signal::Keyframe | Signal::Other => {}
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
        self.update_valves(&state);
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
    /// Só a camada alta: a miniatura tem qualidade fixa.
    pub fn viewer_network(&self) -> Vec<(u32, f64, f64, u64, u32)> {
        let mut out = Vec::new();
        for stats in self.stats_of(Some(Layer::High)) {
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
        self.stats_of(None)
    }

    fn stats_of(&self, layer: Option<Layer>) -> Vec<gst::Structure> {
        let webrtcs: Vec<gst::Element> = self
            .state
            .lock()
            .unwrap()
            .viewers
            .values()
            .filter(|v| layer.is_none_or(|l| v.layer == l))
            .filter_map(|v| v.webrtc.clone())
            .collect();
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

    /// Pede um keyframe (IDR) a todos os encoders.
    pub fn request_keyframe(&self) {
        self.request_keyframe_for(Layer::High);
        if self.low.is_some() {
            self.request_keyframe_for(Layer::Low);
        }
    }

    /// Pede um keyframe (IDR) ao encoder da camada — usado quando alguém entra.
    pub fn request_keyframe_for(&self, layer: Layer) {
        let event = gst_video::UpstreamForceKeyUnitEvent::builder().all_headers(true).build();
        self.video(self.layer(layer)).encoder.static_pad("src").unwrap().send_event(event);
    }

    /// Encoder da miniatura (jam), para configurar na partida.
    pub fn low_encoder(&self) -> Option<&gst::Element> {
        self.low.as_ref().map(|l| &l.encoder)
    }

    /// Pede um keyframe da camada que o espectador `id` recebe.
    pub fn request_keyframe_of(&self, id: u64) {
        let layer = self.state.lock().unwrap().viewers.get(&id).map(|v| v.layer);
        if let Some(layer) = layer {
            self.request_keyframe_for(layer);
        }
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

    /// Contador de buffers que chegam a um elemento nomeado.
    fn counter(streamer: &Streamer, name: &str) -> Arc<AtomicUsize> {
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        let pad = streamer.pipeline.by_name(name).unwrap().static_pad("sink").unwrap();
        pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            c.fetch_add(1, Ordering::Relaxed);
            gst::PadProbeReturn::Ok
        });
        count
    }

    /// Espectador de mentira (sem ramo no pipeline), só para os portões.
    fn fake_viewer(streamer: &Streamer, id: u64, layer: Layer) {
        let mut state = streamer.state.lock().unwrap();
        state.viewers.insert(id, Viewer { bin: gst::Bin::new(), webrtc: None, tee_pads: Vec::new(), layer });
        streamer.update_valves(&state);
    }

    fn clear_viewers(streamer: &Streamer) {
        let mut state = streamer.state.lock().unwrap();
        state.viewers.clear();
        streamer.update_valves(&state);
    }

    /// Jam: o pipeline real (fonte de teste, x264, áudio) com as duas camadas.
    /// Cada camada só codifica com alguém assistindo a ela, o som vai com a
    /// alta, e a miniatura sai a ~15 fps.
    #[test]
    fn jam_layers_encode_only_what_is_watched() {
        gst::init().unwrap();
        if gst::ElementFactory::find("x264enc").is_none() {
            eprintln!("x264enc indisponível; teste ignorado");
            return;
        }
        let settings = crate::capture::Settings {
            bitrate_kbps: 2000,
            fps: 30,
            profile: "constrained-baseline".into(),
            encoder: None,
            monitor: 0,
            monitor_handle: None,
            capture_api: "auto".into(),
            thumbnail: true,
        };
        let description = format!(
            "{} {}",
            crate::capture::test_description("x264enc", &settings),
            crate::capture::audio_description("audiotestsrc is-live=true wave=ticks")
        );
        let streamer = Streamer::new(&description, None, 30).unwrap();
        crate::capture::configure_encoder(&streamer.encoder, &settings);
        crate::capture::configure_encoder(streamer.low_encoder().unwrap(), &crate::capture::thumbnail_settings(&settings));
        let (high, low, audio) = (counter(&streamer, "tee"), counter(&streamer, "ltee"), counter(&streamer, "atee"));
        let measure = || {
            for c in [&high, &low, &audio] {
                c.store(0, Ordering::Relaxed);
            }
            std::thread::sleep(std::time::Duration::from_millis(1500));
            [&high, &low, &audio].map(|c| c.load(Ordering::Relaxed))
        };
        streamer.start().unwrap();

        clear_viewers(&streamer);
        std::thread::sleep(std::time::Duration::from_millis(300));
        let [h, l, a] = measure();
        assert_eq!((h, l, a), (0, 0, 0), "sem ninguém, nada é codificado");

        fake_viewer(&streamer, 1, Layer::Low);
        streamer.request_keyframe();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let [h, l, a] = measure();
        assert_eq!((h, a), (0, 0), "na grade (só miniatura) não sai a camada alta nem som");
        // ~15 quadros/s; pacotes RTP ≥ quadros.
        assert!(l >= 15, "miniatura deveria sair: {l} pacotes em 1,5 s");

        fake_viewer(&streamer, 2, Layer::High);
        std::thread::sleep(std::time::Duration::from_millis(300));
        let [h, l, a] = measure();
        assert!(h > 0 && l > 0 && a > 0, "alta, miniatura e som: {h} {l} {a}");

        // Só a alta: a miniatura para.
        streamer.state.lock().unwrap().viewers.remove(&1);
        streamer.update_valves(&streamer.state.lock().unwrap());
        std::thread::sleep(std::time::Duration::from_millis(300));
        let [h, l, _] = measure();
        assert!(h > 0 && l == 0, "só a alta: {h} {l}");
        streamer.pipeline.set_state(gst::State::Null).unwrap();
    }

    /// Fora da jam (sem miniatura) quem pede a miniatura recebe a alta, e o
    /// som acompanha só a camada alta.
    #[test]
    fn layers_fall_back_and_audio_follows_high() {
        gst::init().unwrap();
        let streamer = Streamer::new(
            "videotestsrc is-live=true ! valve name=valve drop=true ! identity name=enc ! tee name=tee \
             audiotestsrc is-live=true ! valve name=avalve drop=true ! identity ! tee name=atee",
            None,
            30,
        )
        .unwrap();
        assert_eq!(streamer.layer(Layer::Low), Layer::High);
        assert_eq!(streamer.branch_tees(Layer::High).len(), 2);
        fake_viewer(&streamer, 1, Layer::High);
        assert!(!streamer.high.valve.property::<bool>("drop"));
        assert!(!streamer.audio.as_ref().unwrap().1.property::<bool>("drop"));
        clear_viewers(&streamer);
        assert!(streamer.high.valve.property::<bool>("drop"));
        assert!(streamer.audio.as_ref().unwrap().1.property::<bool>("drop"));
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
