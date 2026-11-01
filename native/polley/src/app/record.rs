//! Botão "Gravar" da live: grava o que está sendo transmitido num .mp4 em Vídeos\Polley.

use super::*;
use polley_media::record::Recorded;
use std::path::PathBuf;

/// Pasta Vídeos do usuário (segue o Windows, inclusive se estiver no OneDrive) + \Polley.
fn recordings_dir() -> PathBuf {
    use windows::Win32::UI::Shell::{FOLDERID_Videos, SHGetKnownFolderPath, KNOWN_FOLDER_FLAG};
    let videos = unsafe { SHGetKnownFolderPath(&FOLDERID_Videos, KNOWN_FOLDER_FLAG(0), None) }
        .ok()
        .and_then(|p| {
            let path = unsafe { p.to_string() }.ok();
            unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as *const _)) };
            path
        })
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("USERPROFILE").unwrap_or_default()).join("Videos"));
    videos.join("Polley")
}

fn format_duration(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{:02}:{:02}", s / 60, s % 60)
    }
}

/// Mostra "Gravação salva" (ou nada, se não estava gravando). Usado também ao sair da call.
pub(super) fn show_saved(ui: &slint::Weak<AppWindow>, recorded: Option<Recorded>) {
    let Some(r) = recorded else { return };
    let name = r.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let text = format!("Gravação salva ({}): {name}", format_duration(r.duration));
    let path = r.path.display().to_string();
    let _ = ui.upgrade_in_event_loop(move |ui| {
        ui.set_recording(false);
        ui.set_record_time("".into());
        ui.set_record_message(text.into());
        ui.set_last_recording(path.into());
    });
}

impl App {
    pub async fn toggle_record(self: Arc<Self>) {
        let Some(call) = self.current_call() else { return };
        let Some(recording) = call.recording() else { return };

        if recording.is_recording() {
            let result = tokio::task::spawn_blocking(move || recording.stop()).await;
            match result {
                Ok(Ok(recorded)) => show_saved(&self.ui, recorded),
                Ok(Err(e)) => self.record_error(&format!("{e:#}")),
                Err(_) => self.record_error("a gravação travou"),
            }
            return;
        }

        let name = format!("Polley {}.mp4", Local::now().format("%Y-%m-%d %H-%M-%S"));
        if let Err(e) = call.start_recording(recordings_dir().join(name)) {
            return self.record_error(&format!("{e:#}"));
        }
        self.ui(|ui| {
            ui.set_recording(true);
            ui.set_record_time("00:00".into());
            ui.set_record_message("".into());
        });

        // Relógio "REC 00:12" e aviso se a gravação parar sozinha (disco cheio etc.).
        let app = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if let Some(e) = recording.take_error() {
                    app.record_error(&e);
                    break;
                }
                let Some(elapsed) = recording.elapsed() else { break };
                let text = format_duration(elapsed);
                app.ui(move |ui| ui.set_record_time(text.into()));
            }
        });
    }

    fn record_error(&self, e: &str) {
        let text = format!("A gravação falhou: {e}");
        self.ui(move |ui| {
            ui.set_recording(false);
            ui.set_record_time("".into());
            ui.set_record_message(text.into());
            ui.set_last_recording("".into());
        });
    }

    /// "Mostrar na pasta": abre o Explorer com o arquivo selecionado.
    pub fn show_recording(&self, path: String) {
        let _ = if path.is_empty() {
            std::process::Command::new("explorer.exe").arg(recordings_dir()).spawn()
        } else {
            std::process::Command::new("explorer.exe").arg(format!("/select,{path}")).spawn()
        };
    }
}
