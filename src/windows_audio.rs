//! Loopback somente das árvores de apps permitidos. Nunca abre um endpoint de
//! captura/microfone e nunca cai para loopback global se a seleção falhar.
use crate::audio_policy::{self, Process};
use anyhow::{Context, Result, bail};
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::{HashMap, HashSet};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Diagnostics::ToolHelp::*;
use windows::core::Interface;

static FAILED: OnceLock<Mutex<HashSet<u32>>> = OnceLock::new();

pub fn handle_error(source: Option<&gst::Object>) -> bool {
    let mut current = source.cloned();
    while let Some(object) = current {
        if let Some(pid) = object
            .name()
            .strip_prefix("app-audio-")
            .and_then(|s| s.parse().ok())
        {
            FAILED
                .get_or_init(Mutex::default)
                .lock()
                .unwrap()
                .insert(pid);
            eprintln!("som: captura do processo {pid} interrompida; tentando novamente");
            return true;
        }
        current = object.parent();
    }
    false
}

pub fn source_description() -> String {
    // A fonte silenciosa mantém o relógio e o Opus vivos mesmo sem apps tocando.
    "audiomixer name=appmix force-live=true ignore-inactive-pads=true latency=40000000".into()
}

pub struct AudioRoute {
    stop: mpsc::Sender<()>,
    worker: Option<JoinHandle<()>>,
}

impl Drop for AudioRoute {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct Branch {
    bin: gst::Bin,
    pad: gst::Pad,
    name: String,
}

pub fn start(pipeline: &gst::Pipeline, extra_exclude: &[String]) -> Result<AudioRoute> {
    let factory = gst::ElementFactory::make("wasapi2src").build()?;
    if factory.find_property("loopback-target-pid").is_none() {
        bail!("o áudio por aplicativo requer GStreamer 1.22 ou superior e Windows 11");
    }
    let mixer = pipeline
        .by_name("appmix")
        .context("mixer de aplicativos ausente")?;
    let silence = gst::parse::bin_from_description(
        "audiotestsrc is-live=true wave=silence ! audio/x-raw,rate=48000,channels=2 ! queue",
        true,
    )?;
    pipeline.add(&silence)?;
    silence.link(&mixer)?;
    let pipeline = pipeline.clone();
    let mut exclude: Vec<String> = audio_policy::VOICE_APPS
        .iter()
        .map(|s| s.to_string())
        .collect();
    exclude.extend(
        extra_exclude
            .iter()
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty()),
    );
    let (stop, rx) = mpsc::channel();
    let (ready, ready_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        // As interfaces COM vivem e são descartadas na mesma thread MTA.
        let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok();
        if let Err(err) = initialized {
            let _ = ready.send(Err(err.to_string()));
            return;
        }
        let enumerator: windows::core::Result<IMMDeviceEnumerator> =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) };
        match enumerator {
            Err(err) => {
                let _ = ready.send(Err(err.to_string()));
            }
            Ok(enumerator) => {
                let _ = ready.send(Ok(()));
                let mut branches: HashMap<u32, Branch> = HashMap::new();
                let mut warned = false;
                loop {
                    if pipeline.current_state() == gst::State::Playing {
                        let snapshot = processes().and_then(|processes| {
                            let sessions = unsafe { playback_sessions(&enumerator) }?;
                            Ok((
                                audio_policy::select(
                                    &sessions,
                                    &processes,
                                    &exclude,
                                    std::process::id(),
                                ),
                                processes,
                            ))
                        });
                        let (mut selected, processes) = match snapshot {
                            Ok(snapshot) => {
                                warned = false;
                                snapshot
                            }
                            Err(err) => {
                                if !warned {
                                    eprintln!(
                                        "som: seleção indisponível, mantendo silêncio: {err:#}"
                                    );
                                    warned = true;
                                }
                                (HashSet::new(), HashMap::new())
                            }
                        };
                        let failed: HashSet<_> = FAILED
                            .get_or_init(Mutex::default)
                            .lock()
                            .unwrap()
                            .drain()
                            .collect();
                        selected.retain(|pid| !failed.contains(pid));
                        let remove: Vec<_> = branches
                            .iter()
                            .filter_map(|(&pid, branch)| {
                                (!selected.contains(&pid)
                                    || processes.get(&pid).is_none_or(|p| p.name != branch.name))
                                .then_some(pid)
                            })
                            .collect();
                        // Remova árvores antigas antes de incluir novas (sem som duplicado).
                        for pid in remove {
                            remove_branch(&pipeline, &mixer, branches.remove(&pid).unwrap());
                        }
                        for pid in selected {
                            if branches.contains_key(&pid) {
                                continue;
                            }
                            let name = &processes[&pid].name;
                            match add_branch(&pipeline, &mixer, pid, name) {
                                Ok(branch) => {
                                    println!("som: + {name}");
                                    branches.insert(pid, branch);
                                }
                                Err(err) => {
                                    eprintln!("som: não foi possível capturar {name}: {err:#}")
                                }
                            }
                        }
                    } else {
                        for (_, branch) in branches.drain() {
                            remove_branch(&pipeline, &mixer, branch);
                        }
                    }
                    if rx.recv_timeout(Duration::from_millis(750))
                        != Err(mpsc::RecvTimeoutError::Timeout)
                    {
                        break;
                    }
                }
                for (_, branch) in branches {
                    remove_branch(&pipeline, &mixer, branch);
                }
                drop(enumerator);
            }
        }
        unsafe {
            CoUninitialize();
        }
    });
    let route = AudioRoute {
        stop,
        worker: Some(worker),
    };
    ready_rx
        .recv_timeout(Duration::from_secs(10))
        .context("tempo esgotado ao iniciar áudio do Windows")?
        .map_err(|err| anyhow::anyhow!("não foi possível enumerar áudio do Windows: {err}"))?;
    Ok(route)
}

fn add_branch(
    pipeline: &gst::Pipeline,
    mixer: &gst::Element,
    pid: u32,
    name: &str,
) -> Result<Branch> {
    let bin = gst::parse::bin_from_description(
        &format!(
            "wasapi2src loopback=true loopback-mode=include-process-tree loopback-target-pid={pid} \
         low-latency=true provide-clock=false do-timestamp=true \
         ! audioconvert ! audioresample ! audio/x-raw,rate=48000,channels=2 \
         ! queue max-size-time=100000000 max-size-bytes=0 max-size-buffers=0 leaky=downstream"
        ),
        true,
    )?;
    bin.set_property("name", format!("app-audio-{pid}"));
    pipeline.add(&bin)?;
    let pad = mixer
        .request_pad_simple("sink_%u")
        .context("mixer sem pad")?;
    let result = (|| -> Result<()> {
        bin.static_pad("src")
            .context("captura sem saída")?
            .link(&pad)?;
        bin.sync_state_with_parent()?;
        Ok(())
    })();
    let branch = Branch {
        bin,
        pad,
        name: name.into(),
    };
    if let Err(err) = result {
        remove_branch(pipeline, mixer, branch);
        return Err(err);
    }
    Ok(branch)
}

fn remove_branch(pipeline: &gst::Pipeline, mixer: &gst::Element, branch: Branch) {
    // lock_state evita que um reinício do pipeline religue um ramo em remoção.
    branch.bin.set_locked_state(true);
    let _ = branch.bin.set_state(gst::State::Null);
    if let Some(src) = branch.bin.static_pad("src") {
        let _ = src.unlink(&branch.pad);
    }
    mixer.release_request_pad(&branch.pad);
    let _ = pipeline.remove(&branch.bin);
}

fn processes() -> Result<HashMap<u32, Process>> {
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)?;
        let result = (|| -> Result<_> {
            let mut entry = PROCESSENTRY32W {
                dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            Process32FirstW(snapshot, &mut entry)?;
            let mut result = HashMap::new();
            loop {
                let end = entry
                    .szExeFile
                    .iter()
                    .position(|&ch| ch == 0)
                    .unwrap_or(entry.szExeFile.len());
                result.insert(
                    entry.th32ProcessID,
                    Process {
                        parent: entry.th32ParentProcessID,
                        name: String::from_utf16_lossy(&entry.szExeFile[..end]),
                    },
                );
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
            Ok(result)
        })();
        let _ = CloseHandle(snapshot);
        result
    }
}

unsafe fn playback_sessions(enumerator: &IMMDeviceEnumerator) -> Result<HashSet<u32>> {
    // Apenas eRender: microfones jamais são enumerados.
    unsafe {
        let devices = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
        let mut pids = HashSet::new();
        for i in 0..devices.GetCount()? {
            let device = devices.Item(i)?;
            let manager: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None)?;
            let sessions = manager.GetSessionEnumerator()?;
            for j in 0..sessions.GetCount()? {
                let session = sessions.GetSession(j)?;
                if session.GetState()? == AudioSessionStateExpired {
                    continue;
                }
                let session: IAudioSessionControl2 = session.cast()?;
                // S_FALSE também é sucesso COM; apenas S_OK representa sons do sistema.
                if session.IsSystemSoundsSession() == windows::core::HRESULT(0) {
                    continue;
                }
                if let Ok(pid) = session.GetProcessId() {
                    if pid != 0 {
                        pids.insert(pid);
                    }
                }
            }
        }
        Ok(pids)
    }
}
