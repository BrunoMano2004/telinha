//! Política de captura por árvore de processos, sem dependência de Windows.
//! Quais nomes ficam de fora vem de `audio_rules` (passado como `blocked_name`).
use std::collections::{HashMap, HashSet};


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
    blocked_name: impl Fn(&str) -> bool,
    own_pid: u32,
) -> HashSet<u32> {
    // O app do Telinha (pai do telinha.exe) também fica de fora, com tudo o
    // que ele abre: a janela da jam (WebView2) toca o som das outras telas,
    // que não pode voltar para a transmissão.
    let app_pid = processes
        .get(&own_pid)
        .map(|p| p.parent)
        .filter(|parent| processes.get(parent).is_some_and(|p| p.name.to_lowercase().starts_with("telinha")));
    let blocked: HashSet<_> = processes
        .iter()
        .filter_map(|(&pid, p)| (pid == own_pid || Some(pid) == app_pid || blocked_name(&p.name)).then_some(pid))
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
                |name| ["discord", "spotify"].iter().any(|s| name.to_lowercase().contains(s)),
                7
            ),
            HashSet::from([4])
        );
    }

    /// A janela da jam (WebView2, filha do app) não entra no som; outro app
    /// que também usa WebView2 continua entrando.
    #[test]
    fn excludes_the_telinha_app_and_its_jam_window() {
        let processes: HashMap<_, _> = [
            (1, 0, "explorer.exe"),
            (2, 1, "Telinha Desktop.exe"),
            (3, 2, "telinha.exe"),
            (4, 2, "msedgewebview2.exe"),
            (5, 4, "msedgewebview2.exe"),
            (6, 1, "outroapp.exe"),
            (7, 6, "msedgewebview2.exe"),
        ]
        .into_iter()
        .map(|(pid, parent, name)| (pid, Process { parent, name: name.into() }))
        .collect();
        let sessions = HashSet::from([4, 5, 6, 7]);
        assert_eq!(select(&sessions, &processes, |_| false, 3), HashSet::from([6]));
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
            select(&HashSet::from([1, 2, 3]), &processes, |_| false, 42),
            HashSet::from([3])
        );
    }
}
