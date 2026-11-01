//! Janela "Transmitir sua tela": lista telas e janelas com miniaturas para escolher o que transmitir.
//! As miniaturas só existem enquanto a janela está aberta (fechou → a memória é liberada).

use super::*;
use crate::SourceItem;
use polley_media::capture::{self, Target};
use slint::{Image, Rgba8Pixel, SharedPixelBuffer};

/// Tamanho máximo de cada miniatura (em pixels).
const THUMB: (u32, u32) = (320, 180);

impl App {
    pub async fn open_golive(self: Arc<Self>) {
        // Cada abertura tem um número: miniaturas atrasadas de uma abertura antiga são ignoradas.
        let generation = self.golive_gen.fetch_add(1, Ordering::Relaxed) + 1;
        self.ui(|ui| {
            ui.set_golive_screens(ModelRc::default());
            ui.set_golive_windows(ModelRc::default());
            ui.set_golive_loading(true);
            ui.set_golive_tab_windows(false);
            ui.set_screen_error("".into());
            ui.set_golive_open(true);
        });

        let sources = tokio::task::spawn_blocking(capture::list_sources).await.unwrap_or_default();
        let targets: Vec<Target> = sources.iter().map(|s| s.target).collect();
        let default = sources.iter().find(|s| s.primary).or(sources.first()).map(|s| s.target.id()).unwrap_or_default();
        let rows: Vec<(bool, SharedString, SharedString)> = sources
            .into_iter()
            .map(|s| (matches!(s.target, Target::Window(_)), s.target.id().into(), s.name.into()))
            .collect();
        self.ui(move |ui| {
            let item = |id: &SharedString, name: &SharedString| SourceItem {
                id: id.clone(),
                name: name.clone(),
                thumb: Image::default(),
                has_thumb: false,
            };
            let screens: Vec<SourceItem> = rows.iter().filter(|r| !r.0).map(|r| item(&r.1, &r.2)).collect();
            let windows: Vec<SourceItem> = rows.iter().filter(|r| r.0).map(|r| item(&r.1, &r.2)).collect();
            ui.set_golive_screens(ModelRc::new(VecModel::from(screens)));
            ui.set_golive_windows(ModelRc::new(VecModel::from(windows)));
            ui.set_golive_loading(false);
            // Mantém a escolha anterior se ela ainda existir; senão, a tela principal.
            let current = ui.get_golive_source();
            if !rows.iter().any(|r| r.1 == current) {
                ui.set_golive_source(default.into());
                ui.set_golive_source_window(false);
            }
        });

        // Miniaturas uma por uma, aparecendo conforme ficam prontas.
        for target in targets {
            if self.golive_gen.load(Ordering::Relaxed) != generation {
                return; // janela fechada ou reaberta
            }
            let Ok(Some((w, h, rgba))) =
                tokio::task::spawn_blocking(move || capture::thumbnail(target, THUMB.0, THUMB.1)).await
            else {
                continue;
            };
            let buffer = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&rgba, w, h);
            let (app, id) = (self.clone(), target.id());
            self.ui(move |ui| {
                if app.golive_gen.load(Ordering::Relaxed) != generation {
                    return;
                }
                let model = if matches!(target, Target::Window(_)) { ui.get_golive_windows() } else { ui.get_golive_screens() };
                for i in 0..model.row_count() {
                    if let Some(mut row) = model.row_data(i).filter(|r| r.id == id.as_str()) {
                        row.thumb = Image::from_rgba8(buffer);
                        row.has_thumb = true;
                        model.set_row_data(i, row);
                        break;
                    }
                }
            });
        }
    }

    /// Fecha a janela e solta as miniaturas.
    pub fn close_golive(&self) {
        self.golive_gen.fetch_add(1, Ordering::Relaxed);
        self.ui(|ui| {
            ui.set_golive_open(false);
            ui.set_golive_screens(ModelRc::default());
            ui.set_golive_windows(ModelRc::default());
        });
    }
}
