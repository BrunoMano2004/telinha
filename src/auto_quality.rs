//! Qualidade automática: a cada 2 s olha a rede de quem assiste (perda,
//! retransmissões e jitter dos relatórios RTCP), o tipo de encoder e o uso de
//! CPU do telinha, e ajusta bitrate e fps ao vivo. Como o vídeo é codificado
//! uma vez para todos, vale a conexão do espectador em pior situação.

use serde::Serialize;

pub const TICK: std::time::Duration = std::time::Duration::from_secs(2);

/// Limites conforme o encoder: os de GPU aguentam muito mais.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Limits {
    pub start_kbps: u32,
    pub max_kbps: u32,
    pub max_fps: u32,
}

pub const MIN_KBPS: u32 = 1500;

pub fn limits_for(encoder: &str) -> Limits {
    let software = matches!(encoder, "x264enc" | "mfh264enc" | "openh264enc");
    if software {
        Limits { start_kbps: 4000, max_kbps: 8000, max_fps: 30 }
    } else {
        Limits { start_kbps: 8000, max_kbps: 20000, max_fps: 60 }
    }
}

/// O que foi medido no último intervalo.
#[derive(Clone, Copy, Debug, Default)]
pub struct Sample {
    /// Pior perda entre os espectadores (0..1), incluindo pedidos de retransmissão.
    pub loss: f64,
    /// Pior jitter relatado (segundos): sobe quando a fila da rede enche.
    pub jitter: f64,
    /// Uso de CPU do telinha, em fração de todos os núcleos (0..1).
    pub cpu: f64,
    /// Há alguém assistindo com estatísticas válidas?
    pub has_viewers: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Decision {
    pub kbps: u32,
    pub fps: u32,
    pub reason: &'static str,
}

pub struct Controller {
    limits: Limits,
    kbps: u32,
    fps: u32,
    /// Intervalos seguidos com a rede limpa (para subir com calma).
    clean: u32,
    /// Intervalos de espera depois de uma queda antes de voltar a subir.
    hold: u32,
}

impl Controller {
    pub fn new(limits: Limits) -> Self {
        Self { limits, kbps: limits.start_kbps, fps: limits.max_fps, clean: 0, hold: 0 }
    }

    pub fn current(&self) -> Decision {
        Decision { kbps: self.kbps, fps: self.fps, reason: "início" }
    }

    /// Decide o próximo passo; `None` = manter como está.
    pub fn step(&mut self, s: Sample) -> Option<Decision> {
        if !s.has_viewers {
            self.clean = 0;
            return None;
        }
        let (old_kbps, old_fps) = (self.kbps, self.fps);
        let congested = s.loss > 0.03 || s.jitter > 0.040;
        let reason;
        if congested {
            // Cai rápido: 25% a cada intervalo ruim.
            self.kbps = ((self.kbps as f64 * 0.75) as u32).max(MIN_KBPS);
            self.clean = 0;
            self.hold = 3;
            reason = if s.loss > 0.03 { "perda de pacotes na rede" } else { "rede ficando lenta (atraso subindo)" };
        } else if s.loss < 0.01 && s.jitter < 0.020 {
            // Depois de uma queda, espera 6 s antes de começar a contar.
            if self.hold > 0 {
                self.hold -= 1;
            } else {
                self.clean += 1;
            }
            // Sobe devagar: 10% depois de 6 s seguidos de rede limpa.
            if self.clean >= 3 && self.kbps < self.limits.max_kbps {
                self.kbps = ((self.kbps as f64 * 1.10) as u32).min(self.limits.max_kbps);
                self.clean = 0;
            }
            reason = "rede com folga";
        } else {
            self.clean = 0;
            reason = "rede estável";
        }
        // Com pouco bitrate, menos quadros mantêm cada quadro nítido; com a CPU
        // apertada (encoder por software), menos quadros aliviam o PC.
        self.fps = if s.cpu > 0.75 || self.kbps < 4000 { 30.min(self.limits.max_fps) } else { self.limits.max_fps };
        let reason = if s.cpu > 0.75 && self.fps < old_fps { "CPU sobrecarregada" } else { reason };
        (self.kbps != old_kbps || self.fps != old_fps).then(|| Decision { kbps: self.kbps, fps: self.fps, reason })
    }
}

/// Tempo de CPU gasto pelo processo até agora, em segundos.
pub fn process_cpu_seconds() -> f64 {
    #[cfg(unix)]
    unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut usage);
        let tv = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
        tv(usage.ru_utime) + tv(usage.ru_stime)
    }
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Foundation::FILETIME;
        use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
        let (mut created, mut exited, mut kernel, mut user) =
            (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        if GetProcessTimes(GetCurrentProcess(), &mut created, &mut exited, &mut kernel, &mut user).is_err() {
            return 0.0;
        }
        let secs = |t: FILETIME| ((t.dwHighDateTime as u64) << 32 | t.dwLowDateTime as u64) as f64 / 1e7;
        secs(kernel) + secs(user)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean() -> Sample {
        Sample { loss: 0.0, jitter: 0.002, cpu: 0.1, has_viewers: true }
    }

    #[test]
    fn software_encoders_start_lower() {
        assert_eq!(limits_for("x264enc").max_fps, 30);
        assert_eq!(limits_for("mfh264enc").max_kbps, 8000);
        assert_eq!(limits_for("nvh264enc").max_kbps, 20000);
        assert_eq!(limits_for("nvd3d11h264enc").max_fps, 60);
    }

    #[test]
    fn climbs_slowly_on_clean_network_up_to_the_cap() {
        let mut c = Controller::new(limits_for("nvh264enc"));
        let mut changes = 0;
        for _ in 0..200 {
            if c.step(clean()).is_some() {
                changes += 1;
            }
        }
        assert_eq!(c.kbps, 20000);
        assert_eq!(c.fps, 60);
        // Só sobe a cada 3 intervalos limpos (6 s), não a cada leitura.
        assert!(changes <= 15, "{changes} mudanças");
    }

    #[test]
    fn drops_fast_on_loss_and_waits_before_climbing() {
        let mut c = Controller::new(limits_for("nvh264enc"));
        let lossy = Sample { loss: 0.10, ..clean() };
        let d = c.step(lossy).unwrap();
        assert_eq!(d.kbps, 6000);
        assert!(d.reason.contains("perda"));
        c.step(lossy);
        c.step(lossy);
        assert_eq!(c.kbps, 3375);
        assert_eq!(c.fps, 30, "pouco bitrate: 30 fps para manter cada quadro nítido");
        // Rede volta: 3 intervalos de espera + 3 limpos (12 s) antes de subir.
        for _ in 0..5 {
            assert!(c.step(clean()).is_none());
        }
        assert!(c.step(clean()).is_some());
    }

    #[test]
    fn never_below_minimum() {
        let mut c = Controller::new(limits_for("x264enc"));
        for _ in 0..50 {
            c.step(Sample { loss: 0.5, ..clean() });
        }
        assert_eq!(c.kbps, MIN_KBPS);
    }

    #[test]
    fn rising_jitter_counts_as_congestion() {
        let mut c = Controller::new(limits_for("nvh264enc"));
        let d = c.step(Sample { jitter: 0.08, ..clean() }).unwrap();
        assert!(d.kbps < 8000 && d.reason.contains("atraso"));
    }

    #[test]
    fn busy_cpu_lowers_fps_and_no_viewers_changes_nothing() {
        let mut c = Controller::new(limits_for("nvh264enc"));
        let d = c.step(Sample { cpu: 0.9, ..clean() }).unwrap();
        assert_eq!((d.fps, d.reason), (30, "CPU sobrecarregada"));
        assert!(c.step(Sample { has_viewers: false, ..clean() }).is_none());
    }
}
