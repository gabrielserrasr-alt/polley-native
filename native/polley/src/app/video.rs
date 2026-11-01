//! Live assistida desenhada pela placa de vídeo: uma janelinha encaixada em cima da área da
//! live (ver `polley_media::videoview`). A interface só diz onde ela fica; os quadros vão
//! direto da chamada para a placa.

use super::*;
use polley_media::videoview::{VideoSink, VideoView};

thread_local! {
    static VIEW: RefCell<Option<VideoView>> = const { RefCell::new(None) };
}

/// Liga a janelinha (se ainda não existe) e devolve a entrada de quadros.
/// `None` = sem placa de vídeo utilizável: a live volta a ser desenhada pela interface.
pub(super) fn sink() -> Option<VideoSink> {
    VIEW.with(|v| {
        let mut v = v.borrow_mut();
        if v.is_none() {
            let parent = crate::tray::main_window()?;
            match VideoView::new(parent) {
                Ok(view) => *v = Some(view),
                Err(e) => {
                    eprintln!("live pela placa de vídeo indisponível: {e:#}");
                    return None;
                }
            }
        }
        v.as_ref().map(|view| view.sink())
    })
}

/// Encaixa a janelinha onde a live aparece agora (roda a cada 50 ms na thread da interface).
/// Parou de assistir = fecha tudo (a memória da placa volta).
pub fn tick(ui: &AppWindow) {
    VIEW.with(|v| {
        let mut v = v.borrow_mut();
        if ui.get_watching().is_empty() {
            v.take();
            return;
        }
        let Some(view) = v.as_ref() else { return };
        let scale = ui.window().scale_factor();
        let px = |l: f32| (l * scale).round() as i32;
        let visible = ui.get_video_visible() && !ui.window().is_minimized();
        view.place(px(ui.get_video_x()), px(ui.get_video_y()), px(ui.get_video_w()), px(ui.get_video_h()), visible);
    });
}
