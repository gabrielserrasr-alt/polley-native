//! Parte "ao vivo": login no Supabase (mesmas contas do Polley), pega o token de voz na
//! Edge Function `livekit-token` e publica o vídeo já codificado no LiveKit.

use anyhow::{bail, Context, Result};
use livekit::options::{TrackPublishOptions, VideoCodec, VideoEncoderBackend, VideoEncoding};
use livekit::prelude::*;
use livekit::webrtc::video_source::native::NativeVideoSource;
use livekit::webrtc::audio_source::native::NativeAudioSource;
use livekit::webrtc::audio_source::{AudioSourceOptions, RtcAudioSource};
use livekit::webrtc::video_source::{RtcVideoSource, VideoResolution};

use polley_media::audio;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;

pub struct Config {
    pub supabase_url: String,
    pub anon_key: String,
    pub livekit_url: String,
}

/// Lê o .env do app Electron (procura na pasta atual e nas de cima).
pub fn load_config() -> Result<Config> {
    let mut dir: PathBuf = std::env::current_dir()?;
    let text = loop {
        let candidate = dir.join(".env");
        if let Ok(t) = std::fs::read_to_string(&candidate) {
            break t;
        }
        if !dir.pop() {
            bail!("não achei o arquivo .env do Polley (rode de dentro da pasta do projeto)");
        }
    };
    let vars: HashMap<String, String> = text
        .lines()
        .filter_map(|l| l.trim().trim_start_matches('\u{feff}').split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    let get = |k: &str| vars.get(k).filter(|v| !v.is_empty()).cloned().with_context(|| format!("{k} faltando no .env"));
    Ok(Config {
        supabase_url: get("VITE_SUPABASE_URL")?,
        anon_key: get("VITE_SUPABASE_ANON_KEY")?,
        livekit_url: get("VITE_LIVEKIT_URL")?,
    })
}

fn prompt(label: &str) -> Result<String> {
    print!("{label}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

/// Faz login, deixa escolher o canal de voz e devolve (token do LiveKit, nome do canal).
pub async fn get_voice_token(cfg: &Config, email: &str) -> Result<(String, String)> {
    let http = reqwest::Client::new();
    let password = match std::env::var("POLLEY_PASSWORD") {
        Ok(p) => p,
        Err(_) => rpassword::prompt_password("Senha do Polley (não aparece enquanto digita): ")?,
    };

    let auth: Value = http
        .post(format!("{}/auth/v1/token?grant_type=password", cfg.supabase_url))
        .header("apikey", &cfg.anon_key)
        .json(&json!({ "email": email, "password": password }))
        .send()
        .await?
        .json()
        .await?;
    let access = auth["access_token"]
        .as_str()
        .with_context(|| format!("login falhou: {}", auth["error_description"].as_str().or(auth["msg"].as_str()).unwrap_or("?")))?
        .to_string();

    // Canais de voz dos servidores em que a conta está (a RLS já filtra).
    let channels: Vec<Value> = http
        .get(format!("{}/rest/v1/channels?type=eq.voice&select=id,name,servers(name)", cfg.supabase_url))
        .header("apikey", &cfg.anon_key)
        .bearer_auth(&access)
        .send()
        .await?
        .json()
        .await?;
    if channels.is_empty() {
        bail!("essa conta não está em nenhum servidor com canal de voz");
    }
    println!("\nCanais de voz:");
    for (i, c) in channels.iter().enumerate() {
        println!("  {}) {} / {}", i + 1, c["name"].as_str().unwrap_or("?"), c["servers"]["name"].as_str().unwrap_or("?"));
    }
    let choice: usize = if channels.len() == 1 { 1 } else { prompt("Número do canal: ")?.parse().unwrap_or(1) };
    let channel = channels.get(choice.saturating_sub(1)).context("número inválido")?;
    let channel_id = channel["id"].as_str().context("canal sem id")?;

    let resp: Value = http
        .post(format!("{}/functions/v1/livekit-token", cfg.supabase_url))
        .header("apikey", &cfg.anon_key)
        .bearer_auth(&access)
        .json(&json!({ "channelId": channel_id }))
        .send()
        .await?
        .json()
        .await?;
    let token = resp["token"].as_str().with_context(|| format!("livekit-token recusou: {resp}"))?;
    Ok((token.to_string(), channel["name"].as_str().unwrap_or("?").to_string()))
}

pub struct Published {
    pub room: Room,
    pub video: NativeVideoSource,
    pub mic: Option<LocalAudioTrack>,
}

/// Conecta na sala e publica: tela (H.264 pronto), microfone e som do PC.
#[allow(clippy::too_many_arguments)]
pub async fn publish(
    cfg: &Config,
    token: &str,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
    mic: Option<&audio::Mic>,
    system_audio: Option<std::sync::mpsc::Receiver<Vec<i16>>>,
) -> Result<Published> {
    let (room, _events) = Room::connect(&cfg.livekit_url, token, RoomOptions::default()).await?;
    let source = NativeVideoSource::new_encoded(VideoResolution { width, height });
    let track = LocalVideoTrack::create_video_track("screen", RtcVideoSource::Native(source.clone()));
    room.local_participant()
        .publish_track(
            LocalTrack::Video(track),
            TrackPublishOptions {
                source: TrackSource::Screenshare,
                video_codec: VideoCodec::H264,
                // O vídeo já vem codificado pela placa de vídeo: o LiveKit só empacota e envia.
                video_encoder: VideoEncoderBackend::PreEncoded,
                simulcast: false,
                video_encoding: Some(VideoEncoding { max_bitrate: bitrate as u64, max_framerate: fps as f64 }),
                ..Default::default()
            },
        )
        .await?;

    // Microfone (com cancelamento de eco do WebRTC).
    let mic_track = if let Some(mic) = mic {
        let track = LocalAudioTrack::create_audio_track("mic", mic.audio.rtc_source());
        room.local_participant()
            .publish_track(
                LocalTrack::Audio(track.clone()),
                TrackPublishOptions { source: TrackSource::Microphone, ..Default::default() },
            )
            .await?;
        Some(track)
    } else {
        None
    };

    // Som do PC (sem processamento de voz: é música/jogo, não fala).
    if let Some(rx) = system_audio {
        let opts = AudioSourceOptions { echo_cancellation: false, noise_suppression: false, auto_gain_control: false };
        let src = NativeAudioSource::new(opts, audio::SAMPLE_RATE, audio::CHANNELS, 100);
        let track = LocalAudioTrack::create_audio_track("screen-audio", RtcAudioSource::Native(src.clone()));
        room.local_participant()
            .publish_track(
                LocalTrack::Audio(track),
                TrackPublishOptions { source: TrackSource::ScreenshareAudio, ..Default::default() },
            )
            .await?;
        tokio::spawn(audio::pump_system_audio(rx, src, None));
    }

    Ok(Published { room, video: source, mic: mic_track })
}
