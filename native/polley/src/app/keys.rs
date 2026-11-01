//! Push-to-talk e atalhos globais (mutar / ensurdecer), funcionando com o jogo em primeiro plano.
//!
//! Por que "consultar as teclas a cada 15 ms" (GetAsyncKeyState) e não um gancho de teclado:
//!   - não "rouba" a tecla do jogo (quem aperta F no jogo continua usando o F lá);
//!   - não precisa de gancho global (alguns anti-cheats reclamam de ganchos);
//!   - custa praticamente zero de CPU (uma leitura de estado por tecla configurada).
//! Também funciona com os botões laterais do mouse (Mouse 4 / Mouse 5), favoritos para falar.

use super::*;
use crate::settings::{self, Key};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetKeyNameTextW, MapVirtualKeyW, MAPVK_VK_TO_VSC,
};

const TICK: Duration = Duration::from_millis(15);
/// Depois de soltar a tecla de falar, o microfone fica aberto mais um pouco (não corta a última sílaba).
const PTT_RELEASE: Duration = Duration::from_millis(200);

/// O que a janela "Atalhos" está esperando o usuário apertar.
pub(super) const CAPTURE_NONE: u8 = 0;
const CAPTURE_PTT: u8 = 1;
const CAPTURE_MUTE: u8 = 2;
const CAPTURE_DEAFEN: u8 = 3;

const VK_ESCAPE: u16 = 0x1B;
const VK_SHIFT: u16 = 0x10;
const VK_CONTROL: u16 = 0x11;
const VK_MENU: u16 = 0x12;

fn down(vk: u16) -> bool {
    unsafe { (GetAsyncKeyState(vk as i32) as u16 & 0x8000) != 0 }
}

/// Teclas que não servem sozinhas: botões esquerdo/direito do mouse (o clique em "Mudar" seria
/// capturado) e os modificadores (Ctrl/Shift/Alt/Windows entram como parte do atalho).
fn capturable(vk: u16) -> bool {
    !matches!(vk, 0x01 | 0x02 | 0x10..=0x12 | 0xA0..=0xA5 | 0x5B | 0x5C)
}

/// Tecla + modificadores apertados agora? (Os modificadores precisam bater exatamente.)
fn combo_down(k: &Key) -> bool {
    down(k.vk) && down(VK_CONTROL) == k.ctrl && down(VK_SHIFT) == k.shift && down(VK_MENU) == k.alt
}

/// Nome para mostrar: "Ctrl + Shift + M", "Mouse 4", "F"...
pub(super) fn key_name(k: &Key) -> String {
    let base = match k.vk {
        0x04 => "Botão do meio".to_string(),
        0x05 => "Mouse 4".to_string(),
        0x06 => "Mouse 5".to_string(),
        vk => {
            let mut buf = [0u16; 64];
            let len = unsafe {
                let scan = MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC);
                // Setas, Insert, Home etc. são teclas "estendidas": sem o bit 24 o Windows dá o
                // nome da tecla do teclado numérico.
                let extended = matches!(vk, 0x21..=0x2E | 0x5B | 0x5C | 0x6F | 0x90) as i32;
                GetKeyNameTextW(((scan as i32) << 16) | (extended << 24), &mut buf)
            };
            if len > 0 {
                String::from_utf16_lossy(&buf[..len as usize])
            } else {
                format!("Tecla {vk}")
            }
        }
    };
    let mut parts = Vec::new();
    if k.ctrl {
        parts.push("Ctrl".to_string());
    }
    if k.shift {
        parts.push("Shift".to_string());
    }
    if k.alt {
        parts.push("Alt".to_string());
    }
    parts.push(base);
    parts.join(" + ")
}

impl App {
    /// Liga a thread das teclas (uma vez, depois do login).
    pub(super) fn start_keys(self: &Arc<Self>) {
        if self.keys_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let app = self.clone();
        let handle = tokio::runtime::Handle::current();
        std::thread::Builder::new()
            .name("polley-teclas".into())
            .spawn(move || {
                let _rt = handle.enter(); // para tokio::spawn funcionar a partir desta thread
                let mut ptt_down = false;
                let mut released_at: Option<std::time::Instant> = None;
                let (mut mute_was, mut deafen_was) = (false, false);
                loop {
                    std::thread::sleep(TICK);
                    let capture = app.capturing.load(Ordering::SeqCst);
                    if capture != CAPTURE_NONE {
                        app.capture_tick(capture);
                        continue;
                    }
                    let s = settings::get();

                    // Apertar para falar (só importa numa chamada).
                    if s.push_to_talk && app.current_call().is_some() {
                        let held = s.ptt_key.as_ref().is_some_and(|k| down(k.vk));
                        if held {
                            released_at = None;
                            if !ptt_down {
                                ptt_down = true;
                                app.set_ptt(true);
                            }
                        } else if ptt_down {
                            let since = *released_at.get_or_insert_with(std::time::Instant::now);
                            if since.elapsed() >= PTT_RELEASE {
                                ptt_down = false;
                                released_at = None;
                                app.set_ptt(false);
                            }
                        }
                    } else if ptt_down {
                        ptt_down = false;
                        app.set_ptt(false);
                    }

                    // Atalhos: dispara ao APERTAR (não repete segurando).
                    let mute_now = s.mute_key.as_ref().is_some_and(combo_down);
                    if mute_now && !mute_was {
                        app.toggle_mute();
                    }
                    mute_was = mute_now;
                    let deafen_now = s.deafen_key.as_ref().is_some_and(combo_down);
                    if deafen_now && !deafen_was {
                        app.toggle_deafen();
                    }
                    deafen_was = deafen_now;
                }
            })
            .ok();
    }

    /// Tecla de falar apertada/solta.
    fn set_ptt(self: &Arc<Self>, down: bool) {
        self.ptt_down.store(down, Ordering::SeqCst);
        self.apply_mic();
    }

    /// Esperando a pessoa apertar a tecla nova. Espera soltar tudo antes (o clique em "Mudar").
    fn capture_tick(self: &Arc<Self>, which: u8) {
        if !self.capture_armed.load(Ordering::SeqCst) {
            if !(1u16..=254).any(down) {
                self.capture_armed.store(true, Ordering::SeqCst);
            }
            return;
        }
        if down(VK_ESCAPE) {
            return self.finish_capture(None);
        }
        let Some(vk) = (1u16..=254).find(|&vk| capturable(vk) && vk != VK_ESCAPE && down(vk)) else { return };
        // Tecla de falar é uma tecla só; mutar/ensurdecer aceitam Ctrl/Shift/Alt junto.
        let key = if which == CAPTURE_PTT {
            Key { vk, ..Default::default() }
        } else {
            Key { vk, ctrl: down(VK_CONTROL), shift: down(VK_SHIFT), alt: down(VK_MENU) }
        };
        self.finish_capture(Some(key));
    }

    fn finish_capture(self: &Arc<Self>, key: Option<Key>) {
        let which = self.capturing.swap(CAPTURE_NONE, Ordering::SeqCst);
        if let Some(key) = key {
            let mut s = settings::get();
            match which {
                CAPTURE_PTT => s.ptt_key = Some(key),
                CAPTURE_MUTE => s.mute_key = Some(key),
                CAPTURE_DEAFEN => s.deafen_key = Some(key),
                _ => return,
            }
            settings::set(s);
        }
        self.show_keys();
    }

    /// Botão "Mudar": "ptt" | "mute" | "deafen".
    pub fn capture_key(self: &Arc<Self>, which: &str) {
        let code = match which {
            "ptt" => CAPTURE_PTT,
            "mute" => CAPTURE_MUTE,
            "deafen" => CAPTURE_DEAFEN,
            _ => return,
        };
        self.capture_armed.store(false, Ordering::SeqCst);
        self.capturing.store(code, Ordering::SeqCst);
        self.start_keys();
        let which = which.to_string();
        self.ui(move |ui| ui.set_keys_capturing(which.into()));
    }

    /// Botão "Limpar" de um atalho.
    pub fn clear_key(self: &Arc<Self>, which: &str) {
        let mut s = settings::get();
        match which {
            "mute" => s.mute_key = None,
            "deafen" => s.deafen_key = None,
            _ => return,
        }
        settings::set(s);
        self.show_keys();
    }

    /// "Detecção de voz" ou "Apertar para falar". Vale na hora, até no meio da chamada.
    pub fn set_push_to_talk(self: &Arc<Self>, on: bool) {
        let mut s = settings::get();
        s.push_to_talk = on;
        settings::set(s);
        self.ptt_down.store(false, Ordering::SeqCst);
        self.apply_mic();
        self.show_keys();
    }

    /// "Sons da chamada" ligado/desligado.
    pub fn set_call_sounds(self: &Arc<Self>, on: bool) {
        let mut s = settings::get();
        s.call_sounds = on;
        settings::set(s);
        self.show_keys();
    }

    /// Aba "Windows" das configurações: bandeja e iniciar com o Windows.
    pub fn set_windows_option(self: &Arc<Self>, which: &str, on: bool) {
        let mut s = settings::get();
        match which {
            "autostart" => crate::tray::set_autostart(on),
            "tray" => s.close_to_tray = on,
            "minimized" => s.start_minimized = on,
            _ => {}
        }
        settings::set(s);
        self.show_keys();
    }

    /// Mostra o estado atual nas abas "Atalhos" e "Windows".
    pub(super) fn show_keys(&self) {
        let s = settings::get();
        let (win_auto, win_can_auto) = (crate::tray::autostart_enabled(), crate::tray::can_autostart());
        let (win_tray, win_min) = (s.close_to_tray, s.start_minimized);
        let sounds = s.call_sounds;
        let name = |k: &Option<Key>| k.as_ref().map(key_name).unwrap_or_default();
        let (ptt, ptt_name, mute_name, deafen_name) = (s.push_to_talk, name(&s.ptt_key), name(&s.mute_key), name(&s.deafen_key));
        self.ui(move |ui| {
            ui.set_win_autostart(win_auto);
            ui.set_win_can_autostart(win_can_auto);
            ui.set_win_tray(win_tray);
            ui.set_win_minimized(win_min);
            ui.set_keys_sounds(sounds);
            ui.set_keys_ptt(ptt);
            ui.set_keys_ptt_name(ptt_name.into());
            ui.set_keys_mute_name(mute_name.into());
            ui.set_keys_deafen_name(deafen_name.into());
            ui.set_keys_capturing("".into());
        });
    }

    /// O microfone abre? Mutado/ensurdecido: nunca. Apertar para falar: só com a tecla apertada.
    pub(super) fn mic_should_be_open(&self) -> bool {
        let muted = self.muted.load(Ordering::SeqCst)
            || self.is_deafened()
            || self.srv_muted.load(Ordering::SeqCst);
        !muted && (!settings::get().push_to_talk || self.ptt_down.load(Ordering::SeqCst))
    }

    /// Aplica no microfone da chamada (sem redesenhar nada: é chamado a cada aperto da tecla).
    pub(super) fn apply_mic(&self) {
        if let Some(call) = self.current_call() {
            call.set_muted(!self.mic_should_be_open());
        }
    }
}
