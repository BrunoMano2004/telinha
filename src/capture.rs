//! Montagem do pipeline de captura + codificação, por plataforma.
//!
//! O objetivo é manter o frame na GPU (ou com no máximo uma cópia) e usar
//! sempre o encoder de hardware disponível, caindo para x264 só em último caso.

use anyhow::{Result, bail};
use gstreamer as gst;
use gstreamer::prelude::*;

/// Encoders H.264 em ordem de preferência.
#[cfg(target_os = "linux")]
const ENCODERS: &[&str] = &["nvh264enc", "nvautogpuh264enc", "vah264lpenc", "vah264enc", "x264enc"];
#[cfg(target_os = "windows")]
const ENCODERS: &[&str] = &[
    "nvd3d11h264enc",
    "nvautogpuh264enc",
    "qsvh264enc",
    "amfh264enc",
    "mfh264enc",
    "x264enc",
];

pub struct Settings {
    pub bitrate_kbps: u32,
    pub fps: u32,
    pub profile: String,
    pub encoder: Option<String>,
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    pub monitor: u32,
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    pub monitor_handle: Option<u64>,
    /// Windows: "auto" (WGC só com HDR ligado), "dxgi" ou "wgc".
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    pub capture_api: String,
}

pub fn pick_encoder(forced: Option<&str>) -> Result<String> {
    if let Some(name) = forced {
        if gst::ElementFactory::find(name).is_none() {
            bail!("encoder '{name}' não está disponível nesta máquina");
        }
        return Ok(name.to_string());
    }
    ENCODERS
        .iter()
        .find(|name| {
            if gst::ElementFactory::find(name).is_none() { return false; }
            #[cfg(target_os = "windows")]
            if let Err(err) = probe_windows_encoder(name) {
                eprintln!("encoder {name} indisponível: {err:#}; tentando o próximo");
                return false;
            }
            true
        })
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("nenhum encoder H.264 encontrado (instale os plugins do GStreamer)"))
}

#[cfg(target_os = "windows")]
fn probe_windows_encoder(name: &str) -> Result<()> {
    let settings = Settings { bitrate_kbps: 12000, fps: 30, profile: "constrained-baseline".into(),
        encoder: None, monitor: 0, monitor_handle: None, capture_api: "auto".into() };
    let pipeline = gst::parse::launch(&format!(
        "videotestsrc num-buffers=3 ! video/x-raw,format=BGRx,width=320,height=240,framerate=30/1 \
         ! {} ! {name} name=probe ! video/x-h264,profile=constrained-baseline ! fakesink", windows_convert(name, false)))?
        .downcast::<gst::Pipeline>().map_err(|_| anyhow::anyhow!("pipeline inválido"))?;
    configure_encoder(&pipeline.by_name("probe").unwrap(), &settings);
    let result = (|| -> Result<()> {
        pipeline.set_state(gst::State::Playing)?;
        let message = pipeline.bus().unwrap().timed_pop_filtered(3 * gst::ClockTime::SECOND,
            &[gst::MessageType::Eos, gst::MessageType::Error]).ok_or_else(|| anyhow::anyhow!("encoder não respondeu"))?;
        if let gst::MessageView::Error(error) = message.view() { bail!("{}", error.error()); }
        Ok(())
    })();
    let _ = pipeline.set_state(gst::State::Null);
    result
}

/// A captura roda no máximo do que a qualidade permite (≥ 60 fps); o limite
/// escolhido é aplicado depois, por `Streamer::set_quality`, e pode subir ou
/// descer ao vivo sem reabrir a captura.
fn capture_fps(s: &Settings) -> u32 {
    s.fps.max(60)
}

/// Parte comum: encoder → RTP → tee (cada espectador pendura um ramo no tee).
fn tail(encoder: &str, s: &Settings) -> String {
    format!(
        "{encoder} name=enc \
         ! video/x-h264,profile={profile},stream-format=byte-stream \
         ! h264parse config-interval=-1 \
         ! rtph264pay config-interval=-1 pt=96 mtu=1200 aggregate-mode=zero-latency \
         ! application/x-rtp,media=video,encoding-name=H264,payload=96,clock-rate=90000,\
           rtcp-fb-nack-pli=(boolean)true,rtcp-fb-nack=(boolean)true,rtcp-fb-ccm-fir=(boolean)true \
         ! tee name=tee allow-not-linked=true",
        profile = s.profile,
    )
}

/// `valve` corta o fluxo antes do encoder quando ninguém está assistindo,
/// e a fila de 1 buffer descarta frames antigos se o encoder atrasar.
const GATE: &str = "valve name=valve drop=true ! queue max-size-buffers=1 max-size-bytes=0 max-size-time=0 leaky=downstream";

/// Fonte sintética, para testar rede e navegador sem capturar a tela.
pub fn test_description(encoder: &str, s: &Settings) -> String {
    #[cfg(target_os = "linux")]
    let convert = "videoconvert".to_string();
    #[cfg(target_os = "windows")]
    let convert = windows_convert(encoder, false);
    format!(
        "videotestsrc is-live=true pattern=ball \
         ! video/x-raw,format=BGRx,width=1920,height=1080,framerate={fps}/1 \
         ! {GATE} ! {convert} ! {tail}",
        fps = capture_fps(s),
        tail = tail(encoder, s),
    )
}

#[cfg(target_os = "linux")]
pub fn pipeline_description(fd: i32, node: u32, encoder: &str, s: &Settings) -> String {
    // Encoders NVENC aceitam BGRx/BGRA direto (a conversão para YUV acontece na
    // GPU), então o videoconvert fica em passthrough; ele só trabalha no fallback.
    // always-copy devolve o buffer ao PipeWire na hora: sem isso o encoder o
    // segura e o portal do Hyprland fica sem buffers (cai para ~2 fps).
    // O pipewiresrc duplica o fd a cada partida, então o original serve para
    // religar a captura depois de uma pausa por ociosidade.
    format!(
        "pipewiresrc name=screen fd={fd} path={node} do-timestamp=true always-copy=true keepalive-time=1000 \
         ! video/x-raw,max-framerate={fps}/1 \
         ! videorate drop-only=true max-rate={fps} \
         ! {GATE} ! videoconvert ! {tail}",
        fps = capture_fps(s),
        tail = tail(encoder, s),
    )
}

/// Windows: o monitor está em HDR? A captura DXGI só entrega 8 bits e, com
/// HDR ligado, o Windows corta as cores na conversão (imagem lavada). A
/// Windows Graphics Capture faz a conversão HDR -> SDR com ajuste de tons.
#[cfg(target_os = "windows")]
pub fn monitor_is_hdr(handle: Option<u64>, index: u32) -> bool {
    use windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput6};
    use windows::core::Interface;
    unsafe {
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else { return false };
        let mut position = 0u32;
        let mut adapter_index = 0;
        while let Ok(adapter) = factory.EnumAdapters1(adapter_index) {
            adapter_index += 1;
            let mut output_index = 0;
            while let Ok(output) = adapter.EnumOutputs(output_index) {
                output_index += 1;
                let Ok(desc) = output.cast::<IDXGIOutput6>().and_then(|o| o.GetDesc1()) else { continue };
                let this = match handle {
                    Some(h) => desc.Monitor.0 as usize as u64 == h,
                    None => position == index,
                };
                position += 1;
                if this {
                    return desc.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
                }
            }
        }
        false
    }
}

/// API de captura para o monitor: WGC com HDR (ou se pedida), senão DXGI.
#[cfg(target_os = "windows")]
pub fn capture_api(choice: &str, handle: Option<u64>, index: u32) -> &'static str {
    match choice {
        "dxgi" => "dxgi",
        "wgc" => "wgc",
        _ if monitor_is_hdr(handle, index) => "wgc",
        _ => "dxgi",
    }
}

#[cfg(target_os = "windows")]
pub fn pipeline_description(encoder: &str, s: &Settings) -> String {
    let convert = windows_convert(encoder, true);
    let monitor = s.monitor_handle.map(|handle| format!("monitor-handle={handle}"))
        .unwrap_or_else(|| format!("monitor-index={}", s.monitor));
    let api = capture_api(&s.capture_api, s.monitor_handle, s.monitor);
    if api == "wgc" {
        println!("Captura: Windows Graphics Capture (HDR ligado ou escolhido)");
    }
    format!(
        "d3d11screencapturesrc name=screen {monitor} capture-api={api} show-border=false show-cursor=true \
         ! video/x-raw(memory:D3D11Memory),framerate={fps}/1 \
         ! {GATE} ! {convert} ! {tail}",
        fps = capture_fps(s),
        tail = tail(encoder, s),
    )
}

#[cfg(target_os = "windows")]
fn windows_convert(encoder: &str, gpu_input: bool) -> String {
    // Nem todo encoder do Windows aceita memória D3D11 (Media Foundation,
    // por exemplo). Use as caps anunciadas pelo plugin, inclusive no --test.
    let gpu = gst::ElementFactory::find(encoder).is_some_and(|factory| {
        factory.static_pad_templates().iter().any(|pad| {
            pad.direction() == gst::PadDirection::Sink && pad.caps().to_string().contains("memory:D3D11Memory")
        })
    });
    match (gpu_input, gpu) {
        (true, true) => "d3d11convert ! video/x-raw(memory:D3D11Memory),format=NV12",
        (false, true) => "d3d11upload ! d3d11convert ! video/x-raw(memory:D3D11Memory),format=NV12",
        (true, false) => "d3d11download ! videoconvert ! video/x-raw,format=NV12",
        (false, false) => "videoconvert ! video/x-raw,format=NV12",
    }.into()
}

/// Ramo de áudio (`source` → Opus → RTP → `atee`), montado no mesmo pipeline do
/// vídeo. Tem sua própria `valve`, também fechada sem espectadores. A fonte usa
/// `provide-clock=false` para manter o relógio do sistema no pipeline, o mesmo
/// que o vídeo usa para os timestamps.
pub fn audio_description(source: &str) -> String {
    format!(
        "{source} ! audioconvert ! audioresample ! audio/x-raw,rate=48000,channels=2 \
         ! valve name=avalve drop=true \
         ! opusenc bitrate=128000 frame-size=10 audio-type=generic \
         ! rtpopuspay pt=97 \
         ! application/x-rtp,media=audio,encoding-name=OPUS,payload=97,clock-rate=48000 \
         ! tee name=atee allow-not-linked=true"
    )
}

/// Ajusta o encoder para latência mínima: sem B-frames, CBR com buffer de
/// ~2 frames e GOP infinito (keyframes só quando um espectador entra ou pede).
pub fn configure_encoder(enc: &gst::Element, s: &Settings) {
    let kbps = s.bitrate_kbps;
    let vbv = (kbps * 2 / s.fps.max(1)).max(1).to_string();
    let kbps = kbps.to_string();
    let name = enc.factory().map(|f| f.name().to_string()).unwrap_or_default();

    let props: Vec<(&str, &str)> = if name.starts_with("nv") {
        vec![
            ("preset", "p4"),
            ("tune", "ultra-low-latency"),
            ("rc-mode", "cbr"),
            ("multi-pass", "two-pass-quarter"),
            ("bitrate", &kbps),
            ("vbv-buffer-size", &vbv),
            ("bframes", "0"),
            ("zerolatency", "true"),
            ("gop-size", "-1"),
            ("spatial-aq", "true"),
            ("repeat-sequence-header", "true"),
        ]
    } else if name == "x264enc" {
        vec![
            ("tune", "zerolatency"),
            ("speed-preset", "veryfast"),
            ("bitrate", &kbps),
            ("bframes", "0"),
            ("key-int-max", "0"),
            ("sliced-threads", "true"),
        ]
    } else if name.starts_with("va") {
        vec![("rate-control", "cbr"), ("bitrate", &kbps), ("b-frames", "0"), ("key-int-max", "0")]
    } else if name == "qsvh264enc" {
        vec![("rate-control", "cbr"), ("bitrate", &kbps), ("b-frames", "0"), ("gop-size", "0")]
    } else if name == "amfh264enc" {
        vec![
            ("usage", "ultra-low-latency"),
            ("rate-control", "cbr"),
            ("bitrate", &kbps),
            ("gop-size", "-1"),
        ]
    } else if name == "mfh264enc" {
        vec![("rc-mode", "cbr"), ("bitrate", &kbps), ("low-latency", "true"), ("bframes", "0")]
    } else {
        vec![("bitrate", &kbps)]
    };

    for (key, value) in props {
        try_set(enc, key, value);
    }
}

/// Define uma propriedade a partir de texto só se ela existir e o valor for
/// válido — encoders de fabricantes diferentes expõem conjuntos diferentes.
fn try_set(element: &gst::Element, key: &str, value: &str) {
    let Some(pspec) = element.find_property(key) else { return };
    match gst::glib::Value::deserialize(value, pspec.value_type()) {
        Ok(v) if pspec.value_is_valid(&v) => element.set_property_from_value(key, &v),
        _ => eprintln!("aviso: {key}={value} ignorado no encoder"),
    }
}

/// Altera o bitrate com o pipeline rodando (todos os encoders suportados aceitam).
pub fn set_bitrate(enc: &gst::Element, kbps: u32, fps: u32) {
    try_set(enc, "bitrate", &kbps.to_string());
    try_set(enc, "vbv-buffer-size", &(kbps * 2 / fps.max(1)).max(1).to_string());
}

#[cfg(target_os = "linux")]
pub mod portal {
    //! Captura no Wayland: pede a tela via xdg-desktop-portal e recebe um
    //! stream PipeWire. O token de restauração evita perguntar toda vez.

    use anyhow::{Context, Result};
    use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
    use ashpd::desktop::{PersistMode, Session};
    use std::os::fd::{IntoRawFd, RawFd};
    use std::path::PathBuf;

    pub struct Capture {
        pub fd: RawFd,
        pub node: u32,
        pub size: Option<(i32, i32)>,
        // Mantidos vivos enquanto o programa roda para a sessão não fechar.
        _proxy: Screencast,
        _session: Session<Screencast>,
    }

    fn token_path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join("telinha").join("restore_token"))
    }

    pub async fn open(choose_again: bool) -> Result<Capture> {
        let token = if choose_again {
            None
        } else {
            token_path().and_then(|p| std::fs::read_to_string(p).ok())
        };

        let proxy = Screencast::new().await.context("portal de screencast indisponível")?;
        let session = proxy.create_session(Default::default()).await?;
        proxy
            .select_sources(
                &session,
                SelectSourcesOptions::default()
                    .set_cursor_mode(CursorMode::Embedded)
                    .set_sources(SourceType::Monitor | SourceType::Window)
                    .set_multiple(false)
                    .set_persist_mode(PersistMode::ExplicitlyRevoked)
                    .set_restore_token(token.as_deref().map(str::trim)),
            )
            .await?;
        let streams = proxy
            .start(&session, None, Default::default())
            .await?
            .response()
            .context("compartilhamento de tela cancelado")?;

        if let (Some(t), Some(path)) = (streams.restore_token(), token_path()) {
            let _ = std::fs::create_dir_all(path.parent().unwrap());
            let _ = std::fs::write(path, t);
        }

        let stream = streams.streams().first().context("o portal não retornou nenhum stream")?;
        let fd = proxy.open_pipe_wire_remote(&session, Default::default()).await?;
        Ok(Capture {
            fd: fd.into_raw_fd(),
            node: stream.pipe_wire_node_id(),
            size: stream.size(),
            _proxy: proxy,
            _session: session,
        })
    }

    impl Drop for Capture {
        // Ao trocar de tela a captura antiga sai de cena; o pipewiresrc já
        // duplicou o fd que usou, então o original pode ser fechado.
        fn drop(&mut self) {
            unsafe {
                libc::close(self.fd);
            }
        }
    }
}
