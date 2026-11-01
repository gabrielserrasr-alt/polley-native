//! Configurações de voz e vídeo (microfone, saída de som, câmera, supressão de ruído).
//! Ficam em %APPDATA%\Polley\settings.json (ou settings-<perfil>.json com `--perfil`).
//!
//! Aparelhos são guardados pelo id do Windows (não muda se trocar de porta USB) e pelo nome
//! (para mostrar e para achar de novo se o id sumir). `None` = o padrão do Windows.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

#[derive(Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Device {
    pub id: String,
    pub name: String,
}

/// Uma tecla (código virtual do Windows) com Ctrl/Shift/Alt. Mouse 4/5 = 0x05/0x06.
#[derive(Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Key {
    pub vk: u16,
    #[serde(default)]
    pub ctrl: bool,
    #[serde(default)]
    pub shift: bool,
    #[serde(default)]
    pub alt: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub mic: Option<Device>,
    pub speaker: Option<Device>,
    /// Nome da câmera.
    pub camera: Option<String>,
    /// Supressão de ruído + cancelamento de eco do WebRTC.
    pub noise_suppression: bool,
    /// "Apertar para falar": o microfone só abre com a tecla `ptt_key` apertada.
    pub push_to_talk: bool,
    pub ptt_key: Option<Key>,
    /// Atalhos globais (funcionam dentro do jogo).
    pub mute_key: Option<Key>,
    pub deafen_key: Option<Key>,
    /// Largura da coluna de canais (arrastando a borda).
    pub side_width: Option<u32>,
    /// Sons ao mutar, ensurdecer, entrar e sair da chamada.
    pub call_sounds: bool,
    /// Volume de cada pessoa na chamada (id do usuário → %, de 0 a 200). Quem não está aqui = 100%.
    pub volumes: HashMap<String, u32>,
    /// Pessoas que silenciei só para mim (continuam falando para os outros).
    pub muted_users: HashSet<String>,
    /// Pessoas cuja câmera eu não quero ver (nem baixar).
    pub hidden_cams: HashSet<String>,
    /// Tela da chamada: mostrar quem está sem câmera nem live.
    pub show_no_video: bool,
    /// Tela da chamada: mostrar a minha própria câmera.
    pub show_own_camera: bool,
    /// Câmera em alta qualidade (mais nítida, gasta mais internet).
    pub camera_hq: bool,
    /// Fechar a janela (X) deixa o Polley na bandeja em vez de sair.
    pub close_to_tray: bool,
    /// Quando o Windows abre o Polley, começar só na bandeja (sem mostrar a janela).
    pub start_minimized: bool,
    /// Já ligamos o "iniciar com o Windows" na primeira vez (depois a pessoa decide).
    pub autostart_done: bool,
    // ---- Notificações (DMs e menções)
    pub notif_sound: bool,
    /// Piscar o ícone na barra de tarefas.
    pub notif_flash: bool,
    /// Aviso do Windows (canto da tela) com quem mandou e o começo da mensagem.
    pub notif_popup: bool,
    /// Bolinha vermelha no ícone da barra de tarefas quando há mensagens não lidas.
    pub notif_badge: bool,
    // ---- Aparência e acessibilidade
    /// "dark" (padrão) ou "midnight" (mais escuro).
    pub theme: String,
    /// Mensagens compactas (sem foto, hora antes do nome).
    pub compact: bool,
    /// Tamanho do texto das mensagens (px).
    pub chat_font: u32,
    pub reduce_motion: bool,
    // ---- Idioma e horário
    pub clock_24h: bool,
    // ---- Desenvolvedor
    /// Mostra "Copiar ID" nos menus.
    pub dev_mode: bool,
    // ---- Jogos
    /// Mostrar para os outros o jogo que estou jogando.
    pub share_activity: bool,
    /// Jogos que a pessoa adicionou (além dos que o Polley já conhece).
    pub games: Vec<CustomGame>,
}

/// Jogo adicionado à mão em "Jogos registrados".
#[derive(Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CustomGame {
    /// Nome do programa (ex.: "hades.exe").
    pub exe: String,
    /// Como aparece para os amigos.
    pub name: String,
}

impl Settings {
    /// Volume que realmente toca para uma pessoa (silenciada = 0).
    pub fn volume_of(&self, user: &str) -> u32 {
        if self.muted_users.contains(user) {
            0
        } else {
            self.volumes.get(user).copied().unwrap_or(100)
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mic: None,
            speaker: None,
            camera: None,
            noise_suppression: true,
            push_to_talk: false,
            ptt_key: None,
            mute_key: None,
            deafen_key: None,
            side_width: None,
            call_sounds: true,
            volumes: HashMap::new(),
            muted_users: HashSet::new(),
            hidden_cams: HashSet::new(),
            show_no_video: true,
            show_own_camera: true,
            camera_hq: false,
            close_to_tray: true,
            start_minimized: false,
            autostart_done: false,
            notif_sound: true,
            notif_flash: true,
            notif_popup: true,
            notif_badge: true,
            theme: "dark".into(),
            compact: false,
            chat_font: 15,
            reduce_motion: false,
            clock_24h: true,
            dev_mode: false,
            share_activity: true,
            games: Vec::new(),
        }
    }
}

fn file() -> PathBuf {
    let base = std::env::var("APPDATA").map(PathBuf::from).unwrap_or_else(|_| std::env::temp_dir());
    let mut args = std::env::args().skip_while(|a| a != "--perfil").skip(1);
    let name = match args.next() {
        Some(p) => format!("settings-{}.json", p.replace(['/', '\\', '.'], "")),
        None => "settings.json".into(),
    };
    base.join("Polley").join(name)
}

fn store() -> &'static Mutex<Settings> {
    static S: OnceLock<Mutex<Settings>> = OnceLock::new();
    S.get_or_init(|| {
        let loaded = std::fs::read(file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Mutex::new(loaded)
    })
}

pub fn get() -> Settings {
    store().lock().unwrap().clone()
}

pub fn set(new: Settings) {
    let path = file();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_vec_pretty(&new) {
        let _ = std::fs::write(path, json);
    }
    *store().lock().unwrap() = new;
}

/// Aplica microfone, saída e processamento de voz no áudio do WebRTC. O áudio é um só para o
/// app inteiro, então isso vale na hora, inclusive no meio de uma chamada.
pub fn apply_audio(audio: &livekit::PlatformAudio) {
    let s = get();
    let find = |saved: &Option<Device>, list: Vec<(String, String)>| -> Option<String> {
        let saved = saved.as_ref()?;
        list.iter()
            .find(|(id, _)| *id == saved.id)
            .or_else(|| list.iter().find(|(_, name)| *name == saved.name))
            .map(|(id, _)| id.clone())
    };
    let mics: Vec<_> = audio.recording_devices().map(|d| (d.id.as_str().to_string(), d.name)).collect();
    if let Some(id) = find(&s.mic, mics) {
        if let Some(d) = audio.recording_devices().find(|d| d.id.as_str() == id) {
            let _ = audio.set_recording_device(&d.id);
        }
    }
    let outs: Vec<_> = audio.playout_devices().map(|d| (d.id.as_str().to_string(), d.name)).collect();
    if let Some(id) = find(&s.speaker, outs) {
        if let Some(d) = audio.playout_devices().find(|d| d.id.as_str() == id) {
            let _ = audio.set_playout_device(&d.id);
        }
    }
    let _ = audio.configure_audio_processing(livekit::AudioProcessingOptions {
        echo_cancellation: true,
        noise_suppression: s.noise_suppression,
        auto_gain_control: true,
        prefer_hardware_processing: false,
    });
}

/// Índice da câmera escolhida na lista atual (0 = a primeira, se a escolhida sumiu).
pub fn camera_index(cameras: &[String]) -> usize {
    get().camera.and_then(|name| cameras.iter().position(|c| *c == name)).unwrap_or(0)
}
