//! Janela "Voz e vídeo": escolher microfone, saída de som e câmera.

use super::*;
use crate::settings::{self, Device};

/// Listas mostradas na janela (a posição 0 de mic/saída é "Padrão do Windows").
#[derive(Default)]
pub(super) struct DeviceLists {
    mics: Vec<Device>,
    speakers: Vec<Device>,
    cameras: Vec<String>,
}

const DEFAULT: &str = "Padrão do Windows";

impl App {
    pub async fn open_settings(self: Arc<Self>) {
        self.show_keys();
        tokio::spawn(self.clone().load_prefs());
        self.ui(|ui| {
            ui.set_settings_loading(true);
            ui.set_settings_open(true);
        });
        // Abrir o áudio do WebRTC e o Media Foundation bloqueia um pouco: fora do tokio.
        let lists = tokio::task::spawn_blocking(|| {
            let (mics, speakers) = match livekit::PlatformAudio::new() {
                Ok(a) => (
                    a.recording_devices().map(|d| Device { id: d.id.as_str().to_string(), name: d.name }).collect(),
                    a.playout_devices().map(|d| Device { id: d.id.as_str().to_string(), name: d.name }).collect(),
                ),
                Err(_) => (vec![], vec![]),
            };
            DeviceLists { mics, speakers, cameras: polley_media::camera::list() }
        })
        .await
        .unwrap_or_default();

        let s = settings::get();
        // Posição na lista da janela (0 = padrão; aparelho escolhido que sumiu também vira 0).
        let pos = |list: &[Device], saved: &Option<Device>| -> i32 {
            saved
                .as_ref()
                .and_then(|d| list.iter().position(|x| x.id == d.id).or_else(|| list.iter().position(|x| x.name == d.name)))
                .map(|i| i as i32 + 1)
                .unwrap_or(0)
        };
        let names = |list: &[Device]| -> Vec<SharedString> {
            std::iter::once(DEFAULT.into()).chain(list.iter().map(|d| d.name.clone().into())).collect()
        };
        let (mic_names, mic_pos) = (names(&lists.mics), pos(&lists.mics, &s.mic));
        let (spk_names, spk_pos) = (names(&lists.speakers), pos(&lists.speakers, &s.speaker));
        let cam_names: Vec<SharedString> = if lists.cameras.is_empty() {
            vec!["Nenhuma câmera encontrada".into()]
        } else {
            lists.cameras.iter().map(|c| c.clone().into()).collect()
        };
        let cam_pos = settings::camera_index(&lists.cameras) as i32;
        let noise = s.noise_suppression;
        *self.device_lists.lock().unwrap() = lists;
        self.ui(move |ui| {
            ui.set_settings_mics(ModelRc::new(VecModel::from(mic_names)));
            ui.set_settings_speakers(ModelRc::new(VecModel::from(spk_names)));
            ui.set_settings_cameras(ModelRc::new(VecModel::from(cam_names)));
            ui.set_settings_mic(mic_pos);
            ui.set_settings_speaker(spk_pos);
            ui.set_settings_camera(cam_pos);
            ui.set_settings_noise(noise);
            ui.set_settings_loading(false);
        });
    }

    /// "Salvar": grava no arquivo e aplica o áudio na hora (a câmera vale na próxima vez que ligar).
    /// "Testar microfone" com o microfone/saída escolhidos na tela (mesmo antes de salvar).
    pub fn start_mic_test(self: &Arc<Self>, mic: usize, speaker: usize) {
        use polley_media::mictest::{Event, MicTest};
        let (mic, speaker) = {
            let lists = self.device_lists.lock().unwrap();
            let pick = |list: &[Device], i: usize| if i == 0 { None } else { list.get(i - 1).map(|d| d.id.clone()) };
            (pick(&lists.mics, mic), pick(&lists.speakers, speaker))
        };
        let weak = self.ui.clone();
        let result = MicTest::start(mic, speaker, move |event| {
            let _ = weak.upgrade_in_event_loop(move |ui| match event {
                // Escala em decibéis (igual aos medidores de som): fala normal fica no meio da barra.
                Event::Level(peak) => {
                    let db = 20.0 * peak.max(1e-4).log10();
                    ui.set_mic_level(((db + 60.0) / 60.0).clamp(0.0, 1.0));
                }
                Event::Feedback => {
                    ui.set_mic_monitor(false);
                    ui.set_mic_feedback(true);
                }
            });
        });
        match result {
            Ok(test) => {
                *self.mic_test.lock().unwrap() = Some(test);
                self.ui(|ui| {
                    ui.set_mic_testing(true);
                    ui.set_mic_monitor(false);
                    ui.set_mic_feedback(false);
                });
            }
            Err(e) => {
                let msg = format!("Não consegui abrir o microfone: {e}");
                self.ui(move |ui| ui.set_notice(msg.into()));
            }
        }
    }

    pub fn stop_mic_test(&self) {
        *self.mic_test.lock().unwrap() = None;
        self.ui(|ui| {
            ui.set_mic_testing(false);
            ui.set_mic_monitor(false);
            ui.set_mic_level(0.0);
        });
    }

    pub fn set_mic_monitor(&self, on: bool) {
        if let Some(t) = self.mic_test.lock().unwrap().as_ref() {
            t.set_monitor(on);
        }
        self.ui(move |ui| {
            ui.set_mic_monitor(on);
            if on {
                ui.set_mic_feedback(false);
            }
        });
    }

    pub async fn save_settings(self: Arc<Self>, mic: usize, speaker: usize, camera: usize, noise: bool) {
        self.stop_mic_test();
        let new = {
            let lists = self.device_lists.lock().unwrap();
            let pick = |list: &[Device], i: usize| if i == 0 { None } else { list.get(i - 1).cloned() };
            settings::Settings {
                mic: pick(&lists.mics, mic),
                speaker: pick(&lists.speakers, speaker),
                camera: lists.cameras.get(camera).cloned(),
                noise_suppression: noise,
                ..settings::get() // atalhos e apertar-para-falar não mudam aqui
            }
        };
        settings::set(new);
        let _ = tokio::task::spawn_blocking(|| {
            if let Ok(audio) = livekit::PlatformAudio::new() {
                settings::apply_audio(&audio);
            }
        })
        .await;
        *self.device_lists.lock().unwrap() = DeviceLists::default();
        self.ui(|ui| ui.set_settings_open(false));
    }
}
