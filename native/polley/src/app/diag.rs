//! Diagnóstico de memória (só roda com a variável POLLEY_DIAG=camera ou POLLEY_DIAG=tela).
//! Entra no primeiro canal de voz, liga/desliga a câmera (ou a transmissão) várias vezes e
//! anota a memória a cada volta em %TEMP%\polley-diag.txt.

use super::*;
use std::io::Write;

fn private_mb() -> f64 {
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
    use windows::Win32::System::Threading::GetCurrentProcess;
    let mut c = PROCESS_MEMORY_COUNTERS_EX { cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32, ..Default::default() };
    unsafe {
        let _ = GetProcessMemoryInfo(GetCurrentProcess(), &mut c as *mut _ as *mut PROCESS_MEMORY_COUNTERS, c.cb);
    }
    c.PrivateUsage as f64 / 1_048_576.0
}

fn log(line: &str) {
    let path = std::env::temp_dir().join("polley-diag.txt");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{line}");
    }
}

impl App {
    pub async fn run_diag(self: Arc<Self>, mode: String) {
        let voltas: usize = std::env::var("POLLEY_DIAG_VOLTAS").ok().and_then(|v| v.parse().ok()).unwrap_or(10);
        tokio::time::sleep(Duration::from_secs(10)).await;
        let Ok(list) = self.sb.select::<Vec<Value>>("channels?type=eq.voice&select=id&limit=1").await else {
            return log("sem canal de voz");
        };
        let Some(id) = list.first().and_then(|c| c["id"].as_str()).map(str::to_string) else { return log("sem canal") };
        log(&format!("--- diag {mode} ---  antes da chamada: {:.1} MB", private_mb()));
        self.clone().join_voice(id).await;
        tokio::time::sleep(Duration::from_secs(4)).await;
        log(&format!("na chamada: {:.1} MB", private_mb()));
        for i in 1..=voltas {
            if mode == "tela" {
                self.clone().start_screen(String::new(), 720, 30, false).await;
                tokio::time::sleep(Duration::from_secs(2)).await;
                self.clone().stop_screen().await;
            } else {
                self.clone().toggle_camera().await;
                tokio::time::sleep(Duration::from_secs(2)).await;
                self.clone().toggle_camera().await;
            }
            tokio::time::sleep(Duration::from_millis(1500)).await;
            log(&format!("volta {i:2}: {:.1} MB", private_mb()));
        }
        self.leave_voice();
        tokio::time::sleep(Duration::from_secs(3)).await;
        log(&format!("fora da chamada: {:.1} MB", private_mb()));
    }
}
