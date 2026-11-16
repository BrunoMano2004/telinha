//! `telinha --open-url telinha://join?link=…`: o botão "Compartilhar minha
//! tela" da página da jam abre este link, e o sistema chama o telinha (no
//! Linux pelo `telinha-url.desktop`; no Windows quem abre é o app).
//!
//! Qualquer página pode abrir um link telinha://, então nada é compartilhado
//! sem a pessoa confirmar aqui, vendo em qual endereço vai entrar.
//! Confirmado, quem entra na jam é o card do Caelestia (se o shell estiver
//! aberto), ou este mesmo processo, com uma notificação para parar.

use anyhow::Result;

#[cfg(not(target_os = "linux"))]
pub fn run(_url: &str) -> Result<()> {
    anyhow::bail!("no Windows o link telinha:// abre o app Telinha (instalado pelo instalador)")
}

#[cfg(target_os = "linux")]
pub use linux::run;

#[cfg(target_os = "linux")]
mod linux {
    use super::Result;
    use crate::jam;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::process::{Child, Command, Stdio};
    use std::time::Duration;

    pub fn run(url: &str) -> Result<()> {
        let (link, viewer) = jam::parse_join_url(url)?;
        let host = jam::link_host(&link);
        if !confirm(&host, &link)? {
            eprintln!("cancelado: a tela não foi compartilhada");
            return Ok(());
        }
        match caelestia_join(&link, viewer.as_deref()).as_deref() {
            Some("ok") => return Ok(()),
            Some("busy") => {
                notify(
                    "Você já está transmitindo",
                    "Pare a transmissão atual no Caelestia e clique de novo em \"Compartilhar minha tela\" na página.",
                );
                return Ok(());
            }
            _ => {}
        }
        standalone(&link, viewer.as_deref(), &host)
    }

    /// Pergunta numa notificação com botões (o Caelestia, o mako, o dunst e o
    /// GNOME mostram) ou, sem ela, numa janela do zenity.
    fn confirm(host: &str, link: &str) -> Result<bool> {
        let title = "Compartilhar sua tela na jam?";
        let body = format!(
            "Uma página pediu para o telinha entrar na jam em {host}. Quem estiver lá vai ver a sua tela \
             (e ouvir o som dos apps, sem chamadas de voz)."
        );
        let asked = Command::new("notify-send")
            .args(["--app-name=Telinha", "--icon=video-display", "--urgency=critical"])
            .args(["--action=share=Compartilhar", "--action=cancel=Cancelar", title, &body])
            .stderr(Stdio::null())
            .output();
        if let Ok(out) = asked
            && out.status.success()
        {
            return Ok(String::from_utf8_lossy(&out.stdout).trim() == "share");
        }
        let asked = Command::new("zenity")
            .args(["--question", "--title=Telinha", "--ok-label=Compartilhar", "--cancel-label=Cancelar"])
            .arg(format!("--text={title}\n\n{body}"))
            .stderr(Stdio::null())
            .status();
        match asked {
            Ok(status) => Ok(status.success()),
            Err(_) => anyhow::bail!(
                "sem como pedir confirmação (instale o libnotify ou o zenity); entre pelo terminal: telinha --join \"{link}\""
            ),
        }
    }

    fn notify(title: &str, body: &str) {
        let _ = Command::new("notify-send").args(["--app-name=Telinha", "--icon=video-display", title, body]).status();
    }

    /// Pede ao card do Caelestia para entrar (ele mostra o estado e o botão
    /// de sair). "ok", "busy" ou None se o shell não está aberto.
    fn caelestia_join(link: &str, viewer: Option<&str>) -> Option<String> {
        let out = Command::new("qs")
            .args(["-c", "caelestia", "ipc", "call", "telinha", "join", link, viewer.unwrap_or("")])
            .stderr(Stdio::null())
            .output()
            .ok()?;
        let answer = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (out.status.success() && (answer == "ok" || answer == "busy")).then_some(answer)
    }

    /// Sem o Caelestia: entra na jam por aqui, com uma notificação "Parar"
    /// enquanto a tela estiver sendo compartilhada.
    fn standalone(link: &str, viewer: Option<&str>, host: &str) -> Result<()> {
        let mut command = Command::new(std::env::current_exe()?);
        command.args(["--join", link, "--audio"]).stdin(Stdio::piped());
        if let Some(viewer) = viewer {
            command.args(["--viewer-id", viewer]);
        }
        let mut child = command.spawn()?;
        let mut stdin = child.stdin.take();
        let body = format!("Você está na jam em {host}. A sua tela some da jam quando você parar.");
        let mut id = String::new();
        let mut stopped = false;
        loop {
            let mut args = vec![
                "--app-name=Telinha".to_string(),
                "--icon=video-display".into(),
                "--urgency=critical".into(),
                "--print-id".into(),
                "--action=stop=Parar".into(),
            ];
            if !id.is_empty() {
                args.push(format!("--replace-id={id}"));
            }
            args.extend(["Compartilhando sua tela".to_string(), body.clone()]);
            let Ok(mut note) = Command::new("notify-send").args(&args).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()
            else {
                break; // Sem notificações: só espera a jam terminar.
            };
            let mut out = BufReader::new(note.stdout.take().expect("stdout"));
            let mut first = String::new();
            let _ = out.read_line(&mut first);
            if !first.trim().is_empty() {
                id = first.trim().to_string();
            }
            let action = loop {
                if child.try_wait()?.is_some() {
                    let _ = note.kill();
                    let _ = note.wait();
                    break None;
                }
                if note.try_wait()?.is_some() {
                    let mut rest = String::new();
                    let _ = out.read_to_string(&mut rest);
                    break Some(rest.trim().to_string());
                }
                std::thread::sleep(Duration::from_millis(200));
            };
            match action.as_deref() {
                // A jam acabou (ou o anfitrião tirou você): troca o aviso.
                None => {
                    let mut args = vec!["--app-name=Telinha".to_string(), "--icon=video-display".into()];
                    if !id.is_empty() {
                        args.push(format!("--replace-id={id}"));
                    }
                    args.extend(["Você saiu da jam".to_string(), "A sua tela não está mais sendo compartilhada.".into()]);
                    let _ = Command::new("notify-send").args(&args).status();
                    break;
                }
                Some("stop") => {
                    stop(&mut child, stdin.take());
                    stopped = true;
                    break;
                }
                // Fechou a notificação sem parar: ela volta, porque é o único
                // jeito de parar daqui.
                Some(_) => continue,
            }
        }
        let status = child.wait()?;
        // Parado pela notificação: encerrar à força (preso no seletor de tela,
        // por exemplo) não é erro.
        anyhow::ensure!(stopped || status.success(), "o telinha saiu com erro ({status})");
        Ok(())
    }

    fn stop(child: &mut Child, stdin: Option<std::process::ChildStdin>) {
        if let Some(mut stdin) = stdin {
            let _ = stdin.write_all(b"q\n");
        }
        for _ in 0..50 {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = child.kill();
    }
}
