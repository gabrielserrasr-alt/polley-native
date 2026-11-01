//! Câmera na chamada: ligar/desligar a minha e mostrar a de todo mundo nos quadradinhos.
//! Os quadros chegam de outras threads como pixels e viram `Image` aqui, na thread da
//! interface, num cache por id do usuário (igual às fotos de perfil).

use super::*;
use crate::voice::{CamView, CameraSink};
use crate::Cameras;
use slint::Image;

thread_local! {
    static FRAMES: RefCell<HashMap<String, Image>> = RefCell::new(HashMap::new());
}

/// Liga o `Cameras.lookup` da interface ao cache.
pub(super) fn install(ui: &AppWindow) {
    ui.global::<Cameras>().on_lookup(|id, _version| {
        FRAMES.with(|f| f.borrow().get(id.as_str()).cloned().unwrap_or_default())
    });
}

/// Recebe os quadros de câmera da chamada e entrega para a interface.
pub(super) fn sink(ui: slint::Weak<AppWindow>, view: Arc<CamView>) -> CameraSink {
    Arc::new(move |id, pixels, pending| {
        let view = view.clone();
        let _ = ui.upgrade_in_event_loop(move |ui| {
            FRAMES.with(|f| f.borrow_mut().insert(id, Image::from_rgba8(pixels)));
            let cams = ui.global::<Cameras>();
            cams.set_version(cams.get_version().wrapping_add(1));
            // Tamanho real (em pixels) dos quadradinhos agora, para o próximo quadro vir certo.
            let scale = ui.window().scale_factor();
            let px = |l: f32| (l * scale) as u32;
            view.tile.0.store(px(ui.get_cam_tile_w()), Ordering::Relaxed);
            view.tile.1.store(px(ui.get_cam_tile_h()), Ordering::Relaxed);
            let (bw, bh) = if ui.get_theater() {
                let size = ui.window().size();
                (size.width, size.height)
            } else {
                (px(ui.get_cam_big_w()), px(ui.get_cam_big_h()))
            };
            view.big.0.store(bw, Ordering::Relaxed);
            view.big.1.store(bh, Ordering::Relaxed);
            pending.store(false, Ordering::SeqCst);
        });
    })
}

/// Saiu da chamada: solta os últimos quadros (não ficam ocupando memória).
pub(super) fn clear() {
    FRAMES.with(|f| f.borrow_mut().clear());
}

impl App {
    /// Clicou num quadradinho da chamada: ele fica grande (clicar de novo volta para a grade).
    pub async fn focus_member(self: Arc<Self>, id: String) {
        let Some(call) = self.current_call() else { return };
        let id = if call.cam_view.focus() == id { String::new() } else { id };
        call.cam_view.set_focus(&id);
        self.refresh_voice().await;
    }

    pub async fn toggle_camera(self: Arc<Self>) {
        let Some(call) = self.current_call() else { return };
        self.ui(|ui| ui.set_camera_busy(true));
        let result = if call.camera_on() {
            call.stop_camera().await;
            Ok(false)
        } else {
            call.start_camera().await.map(|_| true)
        };
        let error = result.as_ref().err().map(|e| format!("{e}"));
        let on = result.unwrap_or(false);
        self.ui(move |ui| {
            ui.set_camera_busy(false);
            ui.set_camera_on(on);
            if let Some(e) = error {
                ui.set_last_recording("".into());
                ui.set_record_message(e.into());
            }
        });
        self.refresh_voice().await;
    }
}
