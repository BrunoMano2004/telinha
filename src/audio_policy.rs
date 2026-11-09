//! Política de captura por árvore de processos, sem dependência de Windows.
use std::collections::{HashMap, HashSet};

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub const VOICE_APPS: &[&str] = &[
    "discord",
    "vesktop",
    "webcord",
    "legcord",
    "armcord",
    "equibop",
    "webrtc",
    "teamspeak",
    "ts3client",
    "mumble",
    "zoom",
    "slack",
    "teams",
    "skype",
    "telegram",
    "signal",
    "whatsapp",
    "element",
    "jitsi",
];

#[derive(Clone)]
pub struct Process {
    pub parent: u32,
    pub name: String,
}

fn ancestors(pid: u32, processes: &HashMap<u32, Process>) -> HashSet<u32> {
    let mut seen = HashSet::new();
    let mut next = pid;
    while next != 0 && seen.insert(next) {
        let Some(p) = processes.get(&next) else { break };
        next = p.parent;
    }
    seen
}

pub fn select(
    sessions: &HashSet<u32>,
    processes: &HashMap<u32, Process>,
    exclude: &[String],
    own_pid: u32,
) -> HashSet<u32> {
    let blocked: HashSet<_> = processes
        .iter()
        .filter_map(|(&pid, p)| {
            let name = p.name.to_lowercase();
            (pid == own_pid || exclude.iter().any(|s| name.contains(s))).then_some(pid)
        })
        .collect();
    let trees: HashMap<_, _> = processes
        .keys()
        .map(|&pid| (pid, ancestors(pid, processes)))
        .collect();
    let candidates: HashSet<_> = sessions
        .iter()
        .copied()
        .filter(|pid| {
            let Some(tree) = trees.get(pid) else {
                return false;
            };
            // Não capturar filhos de chamadas nem uma árvore que contenha chamadas.
            tree.is_disjoint(&blocked)
                && !blocked
                    .iter()
                    .any(|b| trees.get(b).is_some_and(|t| t.contains(pid)))
        })
        .collect();
    // include-process-tree já inclui filhos; capturá-los outra vez duplicaria o som.
    candidates
        .iter()
        .copied()
        .filter(|pid| {
            !trees[pid]
                .iter()
                .any(|ancestor| ancestor != pid && candidates.contains(ancestor))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excludes_calls_descendants_ancestors_and_self_without_duplicate_audio() {
        let processes: HashMap<_, _> = [
            (1, 0, "explorer.exe"),
            (2, 1, "Discord.exe"),
            (3, 2, "helper.exe"),
            (4, 1, "firefox.exe"),
            (5, 4, "content.exe"),
            (6, 1, "spotify.exe"),
            (7, 1, "telinha.exe"),
            (8, 7, "helper.exe"),
        ]
        .into_iter()
        .map(|(pid, parent, name)| {
            (
                pid,
                Process {
                    parent,
                    name: name.into(),
                },
            )
        })
        .collect();
        let sessions = (1..=9).collect();
        assert_eq!(
            select(
                &sessions,
                &processes,
                &["discord".into(), "spotify".into()],
                7
            ),
            HashSet::from([4])
        );
    }

    #[test]
    fn tolerates_missing_parents_and_process_tree_cycles() {
        let processes = HashMap::from([
            (
                1,
                Process {
                    parent: 2,
                    name: "app.exe".into(),
                },
            ),
            (
                2,
                Process {
                    parent: 1,
                    name: "helper.exe".into(),
                },
            ),
            (
                3,
                Process {
                    parent: 99,
                    name: "music.exe".into(),
                },
            ),
        ]);
        assert_eq!(
            select(&HashSet::from([1, 2, 3]), &processes, &[], 42),
            HashSet::from([3])
        );
    }
}
