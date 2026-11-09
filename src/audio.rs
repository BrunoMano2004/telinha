//! Som da transmissão sem chamadas de voz e sem microfone (Linux/PipeWire).
//!
//! Capturar o monitor da saída de áudio pega tudo o que você ouve, inclusive a
//! voz de quem está numa call com você, que então volta para a pessoa como eco.
//! Em vez disso, o telinha cria uma saída virtual só dele (um null sink que some
//! junto com o processo), captura o monitor dela e liga a ela uma *cópia* de
//! cada fluxo de reprodução dos apps; o som continua saindo nos seus fones.
//!
//! Ficam de fora:
//! - apps de chamada de voz e fluxos marcados como comunicação;
//! - tudo o que não é fluxo de reprodução de app (microfones, monitores,
//!   loopbacks). Ligações que alguém fizer de outra fonte para a saída virtual
//!   ou para a captura do telinha são desfeitas, então o microfone nunca entra.

use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::{BufReader, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// Apps de chamada de voz (comparado sem maiúsculas, por trecho do nome).
/// "webrtc" pega o "WEBRTC VoiceEngine", nome do fluxo de voz do Discord.
const VOICE_APPS: &[&str] = &[
    "discord", "vesktop", "webcord", "legcord", "armcord", "equibop", "webrtc", "teamspeak", "ts3client",
    "mumble", "zoom", "slack", "teams", "skype", "telegram", "signal", "whatsapp", "element", "jitsi",
];

/// Saída virtual + roteador. Ao ser descartado, os dois processos auxiliares
/// terminam e o PipeWire remove a saída virtual (e as ligações dela).
pub struct AudioRoute {
    sink_name: String,
    _sink_owner: Child,
    _sink_stdin: ChildStdin,
    monitor: Child,
}

impl AudioRoute {
    /// Fonte do GStreamer que captura o que foi ligado à saída virtual.
    pub fn source_description(&self) -> String {
        // dont-fallback/dont-reconnect/dont-move: se a saída virtual sumir, a
        // captura não pode ser religada ao microfone padrão.
        format!(
            "pulsesrc device={}.monitor provide-clock=false buffer-time=40000 latency-time=10000 \
             stream-properties=\"props,node.dont-fallback=true,node.dont-reconnect=true,node.dont-move=true\"",
            self.sink_name
        )
    }
}

impl Drop for AudioRoute {
    fn drop(&mut self) {
        let _ = self.monitor.kill();
        let _ = self.monitor.wait();
        // Fechar o stdin encerra o pw-cli, e com ele a saída virtual.
    }
}

/// Processo filho que morre junto com o telinha, mesmo num SIGKILL.
fn child(program: &str) -> Command {
    let mut cmd = Command::new(program);
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    cmd
}

pub fn start(extra_exclude: &[String]) -> Result<AudioRoute> {
    let sink_name = format!("telinha-{}", std::process::id());

    // O pw-cli fica vivo segurando a saída virtual: sem object.linger, o
    // PipeWire a remove quando o cliente que a criou desconecta.
    let mut owner = child("pw-cli")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("não foi possível rodar o pw-cli (pacote pipewire)")?;
    let mut stdin = owner.stdin.take().unwrap();
    writeln!(
        stdin,
        "create-node adapter {{ factory.name=support.null-audio-sink node.name={sink_name} \
         node.description=\"telinha (transmissão)\" media.class=Audio/Sink audio.position=[FL FR] \
         object.linger=false priority.session=0 priority.driver=0 }}"
    )?;
    stdin.flush()?;

    let mut monitor = child("pw-dump")
        .args(["--monitor", "--no-colors"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("não foi possível rodar o pw-dump (pacote pipewire)")?;
    let out = monitor.stdout.take().unwrap();

    let mut exclude: Vec<String> = VOICE_APPS.iter().map(|s| s.to_string()).collect();
    exclude.extend(extra_exclude.iter().map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()));

    let (ready_tx, ready_rx) = mpsc::channel();
    let mut router = Router {
        sink_name: sink_name.clone(),
        pid: std::process::id().to_string(),
        exclude,
        objects: HashMap::new(),
        pending: HashSet::new(),
        announced: HashSet::new(),
        ready: Some(ready_tx),
    };
    std::thread::spawn(move || {
        let batches = serde_json::Deserializer::from_reader(BufReader::new(out)).into_iter::<Vec<Value>>();
        for batch in batches.map_while(Result::ok) {
            router.apply(batch);
            router.sync();
        }
    });

    // A captura só pode começar depois que a saída virtual existir.
    if ready_rx.recv_timeout(Duration::from_secs(5)).is_err() {
        let _ = monitor.kill();
        bail!("a saída de áudio virtual do telinha não apareceu no PipeWire");
    }
    Ok(AudioRoute { sink_name, _sink_owner: owner, _sink_stdin: stdin, monitor })
}

struct Router {
    sink_name: String,
    pid: String,
    exclude: Vec<String>,
    /// Nós, portas e ligações do PipeWire, por id.
    objects: HashMap<u32, Value>,
    /// Ligações pedidas ao pw-link que ainda não apareceram (evita repetir).
    pending: HashSet<(u32, u32)>,
    /// Apps já anunciados no terminal (nome, incluído?): um aviso por app.
    announced: HashSet<(String, bool)>,
    ready: Option<mpsc::Sender<()>>,
}

fn prop(obj: &Value, key: &str) -> String {
    match &obj["info"]["props"][key] {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        v => v.to_string(),
    }
}

fn id_of(v: &Value) -> Option<u32> {
    v.as_u64().map(|n| n as u32)
}

/// Canais do app → canais da saída virtual (estéreo).
fn targets(channel: &str) -> &'static [&'static str] {
    match channel {
        "FL" | "SL" | "RL" | "FLC" | "AUX0" => &["FL"],
        "FR" | "SR" | "RR" | "FRC" | "AUX1" => &["FR"],
        "MONO" | "FC" | "RC" => &["FL", "FR"],
        _ => &[],
    }
}

impl Router {
    fn apply(&mut self, batch: Vec<Value>) {
        for obj in batch {
            let Some(id) = id_of(&obj["id"]) else { continue };
            if obj["info"].is_null() {
                self.objects.remove(&id);
                self.pending.retain(|&(o, i)| o != id && i != id);
                continue;
            }
            let kind = obj["type"].as_str().unwrap_or_default();
            if kind.ends_with(":Node") || kind.ends_with(":Port") || kind.ends_with(":Link") {
                self.objects.insert(id, obj);
            }
        }
    }

    fn of_type<'a>(&'a self, suffix: &'a str) -> impl Iterator<Item = (u32, &'a Value)> + 'a {
        self.objects.iter().filter(move |(_, o)| o["type"].as_str().is_some_and(|t| t.ends_with(suffix))).map(|(&id, o)| (id, o))
    }

    /// Fluxo de reprodução de um app que pode ir para a transmissão?
    /// `None` = nem é fluxo de app; `Some(false)` = app excluído.
    fn allowed(&self, node: &Value) -> Option<bool> {
        if prop(node, "media.class") != "Stream/Output/Audio" {
            return None;
        }
        let node_name = prop(node, "node.name").to_lowercase();
        // Loopbacks e cancelamento de eco podem estar carregando um microfone.
        if node_name.contains("loopback") || node_name.contains("echo-cancel") {
            return None;
        }
        let role = prop(node, "media.role").to_lowercase();
        if role == "communication" || role == "phone" {
            return Some(false);
        }
        let names = ["application.name", "application.process.binary", "application.id"]
            .iter()
            .map(|k| prop(node, k).to_lowercase())
            .chain([node_name])
            .collect::<Vec<_>>();
        let excluded = names.iter().any(|n| !n.is_empty() && self.exclude.iter().any(|e| n.contains(e.as_str())));
        Some(!excluded)
    }

    fn sync(&mut self) {
        let Some(sink) = self.of_type(":Node").find(|(_, n)| prop(n, "node.name") == self.sink_name).map(|(id, _)| id) else {
            return;
        };
        if let Some(tx) = self.ready.take() {
            let _ = tx.send(());
        }

        // Portas por nó: (nó, direção) → [(canal, id da porta)].
        let mut ports: HashMap<(u32, String), Vec<(String, u32)>> = HashMap::new();
        for (id, p) in self.of_type(":Port") {
            if prop(p, "port.monitor") == "true" {
                continue;
            }
            let Ok(node) = prop(p, "node.id").parse::<u32>() else { continue };
            let dir = p["info"]["direction"].as_str().unwrap_or_default().to_string();
            ports.entry((node, dir)).or_default().push((prop(p, "audio.channel"), id));
        }
        let sink_inputs: HashMap<String, u32> =
            ports.get(&(sink, "input".into())).into_iter().flatten().cloned().collect();

        // Nós de captura do próprio telinha (o pulsesrc): só podem ouvir a saída virtual.
        let own_captures: HashSet<u32> = self
            .of_type(":Node")
            .filter(|(_, n)| prop(n, "media.class") == "Stream/Input/Audio" && prop(n, "application.process.id") == self.pid)
            .map(|(id, _)| id)
            .collect();

        let mut desired = HashSet::new();
        let mut verdicts = Vec::new();
        for (id, node) in self.of_type(":Node") {
            let Some(ok) = self.allowed(node) else { continue };
            let name = [prop(node, "application.name"), prop(node, "node.name")]
                .into_iter()
                .find(|n| !n.is_empty())
                .unwrap_or_else(|| format!("fluxo {id}"));
            verdicts.push((id, ok, name));
            if !ok {
                continue;
            }
            for (channel, out_port) in ports.get(&(id, "output".into())).into_iter().flatten() {
                for target in targets(channel) {
                    if let Some(&in_port) = sink_inputs.get(*target) {
                        desired.insert((*out_port, in_port));
                    }
                }
            }
        }

        let mut existing = HashSet::new();
        let mut unwanted = Vec::new();
        for (link_id, link) in self.of_type(":Link") {
            let info = &link["info"];
            let (Some(out_node), Some(out_port), Some(in_node), Some(in_port)) = (
                id_of(&info["output-node-id"]),
                id_of(&info["output-port-id"]),
                id_of(&info["input-node-id"]),
                id_of(&info["input-port-id"]),
            ) else {
                continue;
            };
            existing.insert((out_port, in_port));
            let into_sink = in_node == sink && !desired.contains(&(out_port, in_port));
            let into_capture = own_captures.contains(&in_node) && out_node != sink;
            if into_sink || into_capture {
                unwanted.push(link_id);
            }
        }

        for (_, ok, name) in verdicts {
            if self.announced.insert((name.clone(), ok)) {
                if ok {
                    println!("som: + {name}");
                } else {
                    println!("som: ignorando {name} (chamada de voz)");
                }
            }
        }
        for link in unwanted {
            let _ = Command::new("pw-link").args(["-d", &link.to_string()]).stderr(Stdio::null()).status();
        }
        for pair in desired {
            if existing.contains(&pair) {
                self.pending.remove(&pair);
            } else if self.pending.insert(pair) {
                let _ = Command::new("pw-link")
                    .args([pair.0.to_string(), pair.1.to_string()])
                    .stderr(Stdio::null())
                    .status();
            }
        }
    }
}
