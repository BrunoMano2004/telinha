//! Quais apps entram no som da transmissão. Compartilhado pelo roteador do
//! Linux (PipeWire) e pelo do Windows (WASAPI), e alterável ao vivo pelo
//! comando `app on|off <nome>`. As interfaces recebem a lista de apps tocando
//! som pelo evento `@apps`.

use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, OnceLock};

/// Apps de chamada de voz, fora por padrão: a voz de quem está na call
/// voltaria para a pessoa como eco. Comparado por trecho do nome, sem
/// maiúsculas; "webrtc" pega o "WEBRTC VoiceEngine", o fluxo de voz do Discord.
pub const VOICE_APPS: &[&str] = &[
    "discord", "vesktop", "webcord", "legcord", "armcord", "equibop", "webrtc", "teamspeak", "ts3client",
    "mumble", "zoom", "slack", "teams", "skype", "telegram", "signal", "whatsapp", "element", "jitsi",
];

#[derive(Default)]
struct Rules {
    /// Trechos de nome excluídos por padrão (chamadas + --audio-exclude).
    patterns: Vec<String>,
    /// Escolhas explícitas por nome exato (sem maiúsculas); vencem os padrões.
    overrides: HashMap<String, bool>,
    last_report: String,
    on_change: Option<Arc<dyn Fn() + Send + Sync>>,
}

fn rules() -> &'static Mutex<Rules> {
    static RULES: OnceLock<Mutex<Rules>> = OnceLock::new();
    RULES.get_or_init(Mutex::default)
}

fn clean(name: &str) -> String {
    name.trim().to_lowercase()
}

pub fn init(exclude: &[String], include: &[String]) {
    let mut r = rules().lock().unwrap();
    r.patterns = VOICE_APPS.iter().map(|s| s.to_string()).collect();
    r.patterns.extend(exclude.iter().map(|s| clean(s)).filter(|s| !s.is_empty()));
    r.overrides = include.iter().map(|s| clean(s)).filter(|s| !s.is_empty()).map(|s| (s, true)).collect();
}

/// Chamado depois de cada mudança de regra (o Linux religa o grafo na hora;
/// o Windows não precisa, ele reavalia a cada 750 ms).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn on_change(f: impl Fn() + Send + Sync + 'static) {
    rules().lock().unwrap().on_change = Some(Arc::new(f));
}

/// Veredito para um fluxo, dado os nomes pelos quais ele é conhecido.
/// `voice_hint`: o próprio fluxo se declara de comunicação (ex.: media.role).
/// Retorna (entra na transmissão?, é app de chamada/excluído por padrão?).
pub fn verdict(names: &[&str], voice_hint: bool) -> (bool, bool) {
    let r = rules().lock().unwrap();
    let names: Vec<String> = names.iter().map(|n| clean(n)).filter(|n| !n.is_empty()).collect();
    let default_excluded = voice_hint || names.iter().any(|n| r.patterns.iter().any(|p| n.contains(p.as_str())));
    let chosen = names.iter().find_map(|n| r.overrides.get(n).copied());
    (chosen.unwrap_or(!default_excluded), default_excluded)
}

/// Liga (`on`) ou desliga um app pelo nome mostrado em `@apps`.
pub fn set(name: &str, on: bool) {
    let callback = {
        let mut r = rules().lock().unwrap();
        r.overrides.insert(clean(name), on);
        r.last_report.clear(); // reenvia a lista com o novo estado
        r.on_change.clone()
    };
    // Fora do lock: o callback consulta `verdict`.
    if let Some(callback) = callback {
        callback();
    }
}

#[derive(Serialize)]
struct AppEntry<'a> {
    name: &'a str,
    on: bool,
    voice: bool,
}

/// Publica `@apps` (só quando a lista muda): apps tocando som agora.
/// `apps`: (nome mostrado, entra?, é chamada?).
pub fn report(apps: impl IntoIterator<Item = (String, bool, bool)>) {
    let mut merged: BTreeMap<String, (bool, bool)> = BTreeMap::new();
    for (name, on, voice) in apps {
        let entry = merged.entry(name).or_insert((false, false));
        entry.0 |= on;
        entry.1 |= voice;
    }
    let list: Vec<_> = merged.iter().map(|(name, &(on, voice))| AppEntry { name, on, voice }).collect();
    let json = serde_json::to_string(&list).unwrap_or_default();
    let mut r = rules().lock().unwrap();
    if r.last_report != json {
        println!("@apps {json}");
        r.last_report = json;
    }
}
