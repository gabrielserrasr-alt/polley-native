//! Chamada de voz (sala do LiveKit). É a mesma sala que o app Electron usa (`ch:<id do canal>`),
//! então os dois apps se escutam e se assistem normalmente.
//!
//! O `PlatformAudio` (microfone + fone, com cancelamento de eco) só existe enquanto a chamada
//! está aberta: fora da call o app não segura nenhum dispositivo de áudio nem a RAM dele.
//!
//! Assinaturas manuais (auto_subscribe desligado): o microfone de todo mundo é recebido sempre,
//! mas o vídeo/som de uma live só é baixado enquanto você está assistindo — igual ao Discord.
//! Sem isso, cada live aberta na call gastaria internet e CPU (decodificação) à toa.

use anyhow::{anyhow, bail, Result};
use futures_util::StreamExt;
use livekit::options::{TrackPublishOptions, VideoCodec, VideoEncoderBackend, VideoEncoding};
use livekit::prelude::*;
use livekit::webrtc::audio_source::native::NativeAudioSource;
use livekit::webrtc::audio_source::{AudioSourceOptions, RtcAudioSource};
use livekit::webrtc::audio_stream::native::NativeAudioStream;
use livekit::webrtc::video_frame::native::VideoFrameBufferExt;
use livekit::webrtc::video_frame::VideoFormatType;
use livekit::webrtc::video_source::native::NativeVideoSource;
use livekit::webrtc::video_source::{RtcVideoSource, VideoResolution};
use livekit::webrtc::video_stream::native::NativeVideoStream;
use livekit::PlatformAudio;
use livekit::track::VideoQuality;
use livekit::webrtc::video_frame::VideoBuffer;
use polley_media::audio;
use polley_media::camera::CameraInfo;
use polley_media::camstream::{self, CameraStream};
use polley_media::capture::Target;
use polley_media::playback::Player;
use polley_media::record::{Recorded, Recording};
use polley_media::stream::{Sink, Stream, StreamConfig, StreamInfo};
use slint::{Rgba8Pixel, SharedPixelBuffer};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::UnboundedReceiver;

pub const LIVEKIT_URL: &str = env!("POLLEY_LIVEKIT_URL");

/// Recebe cada quadro da live (já no tamanho da tela) — é quem mostra na interface.
pub type FrameSink = Arc<dyn Fn(SharedPixelBuffer<Rgba8Pixel>) + Send + Sync>;

/// Recebe quadros de câmera (id do usuário, imagem). O `AtomicBool` fica ligado até a
/// interface desenhar o quadro: enquanto isso, os próximos daquela câmera são pulados.
pub type CameraSink = Arc<dyn Fn(String, SharedPixelBuffer<Rgba8Pixel>, Arc<AtomicBool>) + Send + Sync>;

/// Nos quadradinhos da grade a câmera aparece a 15 quadros por segundo (redesenhar a interface
/// 30x por segundo por pessoa gastaria CPU à toa); a câmera em destaque aparece a 30.
const CAMERA_FRAME_GAP: std::time::Duration = std::time::Duration::from_millis(66);
const CAMERA_FOCUS_GAP: std::time::Duration = std::time::Duration::from_millis(33);

/// Onde as câmeras aparecem na tela: o quadro é reduzido para o tamanho REAL do quadradinho
/// (nem maior, que gastaria CPU; nem menor, que ficaria borrado ao esticar).
pub struct CamView {
    /// Pessoa em destaque (clicou no quadradinho dela); "" = grade normal.
    focus: Mutex<String>,
    /// Tamanho em pixels de um quadradinho da grade.
    pub tile: (AtomicU32, AtomicU32),
    /// Tamanho em pixels da câmera em destaque (ou da tela toda, em tela cheia).
    pub big: (AtomicU32, AtomicU32),
}

impl Default for CamView {
    fn default() -> Self {
        Self {
            focus: Mutex::default(),
            tile: (AtomicU32::new(640), AtomicU32::new(360)),
            big: (AtomicU32::new(1280), AtomicU32::new(720)),
        }
    }
}

impl CamView {
    pub fn focus(&self) -> String {
        self.focus.lock().unwrap().clone()
    }

    pub fn set_focus(&self, id: &str) {
        *self.focus.lock().unwrap() = id.to_string();
    }

    /// Para a câmera de `id`: tamanho máximo, se ela preenche o quadradinho cortando as
    /// bordas (`cover`) e o intervalo entre quadros.
    fn layout(&self, id: &str) -> ((u32, u32), bool, std::time::Duration) {
        let read = |p: &(AtomicU32, AtomicU32)| (p.0.load(Ordering::Relaxed).clamp(2, 3840), p.1.load(Ordering::Relaxed).clamp(2, 2160));
        if *self.focus.lock().unwrap() == id {
            (read(&self.big), false, CAMERA_FOCUS_GAP)
        } else {
            (read(&self.tile), true, CAMERA_FRAME_GAP)
        }
    }
}

/// Reduz um quadro para o tamanho `max` e converte para RGBA (o que a interface desenha).
/// `cover`: o quadro vai preencher o quadradinho (as sobras são cortadas), então basta um dos
/// lados ter o tamanho certo; senão, o quadro inteiro tem que caber.
fn to_rgba(buffer: &dyn VideoBuffer, max: (u32, u32), cover: bool) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let (fw, fh) = (buffer.width(), buffer.height());
    let (sx, sy) = (max.0 as f64 / fw as f64, max.1 as f64 / fh as f64);
    let scale = if cover { sx.max(sy) } else { sx.min(sy) }.min(1.0);
    let tw = (((fw as f64 * scale) as u32) & !1).max(2);
    let th = (((fh as f64 * scale) as u32) & !1).max(2);
    let mut i420 = if let Some(b) = buffer.as_i420() {
        b.to_i420()
    } else if let Some(b) = buffer.as_native() {
        b.to_i420()
    } else if let Some(b) = buffer.as_nv12() {
        b.to_i420()
    } else {
        return None;
    };
    let scaled = if (tw, th) != (fw, fh) { i420.scale(tw as i32, th as i32) } else { i420 };
    let mut image = SharedPixelBuffer::<Rgba8Pixel>::new(tw, th);
    scaled.to_argb(VideoFormatType::ABGR, image.make_mut_bytes(), tw * 4, tw as i32, th as i32);
    Some(image)
}

/// Minha câmera ligada.
/// Minha câmera ligada: a câmera + o nosso encoder H.264 (fechados ao desligar = RAM volta).
struct Cam {
    stream: CameraStream,
}

/// A trilha da câmera na sala (já comprimida por nós: o WebRTC só empacota e envia).
/// Fica publicada, em pausa, depois de desligar: religar reaproveita em vez de criar outra.
struct CamTrack {
    publication: LocalTrackPublication,
    source: NativeVideoSource,
    size: (u32, u32),
}

/// Minha transmissão em andamento.
struct Screen {
    stream: Stream,
    tracks: Vec<TrackSid>,
    audio_stop: Arc<AtomicBool>,
    /// A live tem o som do PC (a gravação também terá).
    with_audio: bool,
}

/// Pessoa com volume diferente de 100%: a voz dela toca pelo nosso próprio tocador.
struct Boosted {
    /// Trilha que está sendo tocada (se a pessoa reconectar, vem outra).
    sid: String,
    player: Arc<Player>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Boosted {
    fn drop(&mut self) {
        // Parar a tarefa solta o NativeAudioStream, que se desliga da trilha sozinho.
        self.task.abort();
    }
}

/// Live que estou assistindo.
struct Watch {
    identity: String,
    sink: FrameSink,
    /// Janelinha da placa de vídeo (quando existe, a interface não desenha a live).
    gpu: Option<polley_media::videoview::VideoSink>,
}

pub struct Call {
    pub channel_id: String,
    room: Room,
    mic: LocalAudioTrack,
    screen: Mutex<Option<Screen>>,
    cam: Mutex<Option<Cam>>,
    cam_track: Mutex<Option<CamTrack>>,
    /// Para onde vão os quadros das câmeras (a minha prévia e a dos outros).
    camera_sink: Mutex<Option<CameraSink>>,
    /// Tamanho dos quadradinhos e quem está em destaque (a interface atualiza).
    pub cam_view: Arc<CamView>,
    watch: Mutex<Option<Watch>>,
    /// Muda a cada troca de live assistida: o decodificador antigo percebe e para.
    watch_gen: Arc<AtomicU64>,
    /// Tamanho (em pixels) da área onde a live aparece; o quadro é reduzido para caber nela.
    pub viewport: Arc<(AtomicU32, AtomicU32)>,
    /// A interface ainda não desenhou o último quadro? Então o próximo é descartado.
    pub frame_pending: Arc<AtomicBool>,
    /// Medidor da live assistida: quadros recebidos, quadros mostrados e resolução original.
    pub watch_stats: Arc<WatchStats>,
    boosted: Mutex<HashMap<String, Boosted>>,
    deafened: Arc<AtomicBool>,
    // Mantém microfone/saída ligados enquanto a chamada existir.
    _audio: PlatformAudio,
}

/// Uma pessoa na chamada (id = id do usuário no Supabase).
pub struct Member {
    pub id: String,
    pub is_me: bool,
    pub speaking: bool,
    pub muted: bool,
    pub live: bool,
    pub camera: bool,
}

impl Call {
    pub async fn connect(
        token: &str,
        channel_id: String,
        muted: bool,
        deafened: bool,
    ) -> Result<(Self, UnboundedReceiver<RoomEvent>)> {
        // Precisa existir ANTES de conectar, senão a voz dos outros não toca.
        let audio = PlatformAudio::new().map_err(|e| anyhow!("não achei microfone/fone de ouvido ({e})"))?;
        // Microfone/saída escolhidos em "Voz e vídeo" + supressão de ruído.
        crate::settings::apply_audio(&audio);

        let mut options = RoomOptions::default();
        options.auto_subscribe = false;
        let (room, events) = Room::connect(LIVEKIT_URL, token, options)
            .await
            .map_err(|e| anyhow!("não consegui entrar na chamada ({e})"))?;

        let mic = LocalAudioTrack::create_audio_track("mic", audio.rtc_source());
        if muted {
            mic.mute();
        }
        // Silenciado por um moderador: o servidor recusa o microfone, mas a pessoa entra e ouve.
        // (`ensure_mic` publica de novo quando o moderador liberar.)
        if let Err(e) = room
            .local_participant()
            .publish_track(
                LocalTrack::Audio(mic.clone()),
                TrackPublishOptions { source: TrackSource::Microphone, ..Default::default() },
            )
            .await
        {
            eprintln!("microfone não publicado: {e}");
        }

        let call = Self {
            channel_id,
            room,
            mic,
            screen: Mutex::new(None),
            cam: Mutex::new(None),
            cam_track: Mutex::new(None),
            camera_sink: Mutex::new(None),
            cam_view: Arc::default(),
            watch: Mutex::new(None),
            watch_gen: Arc::new(AtomicU64::new(0)),
            viewport: Arc::new((AtomicU32::new(1280), AtomicU32::new(720))),
            frame_pending: Arc::new(AtomicBool::new(false)),
            watch_stats: Arc::default(),
            boosted: Mutex::default(),
            deafened: Arc::default(),
            _audio: audio,
        };
        call.sync_subscriptions();
        call.set_deafened(deafened);
        Ok((call, events))
    }

    /// Quem está na sala: eu primeiro, depois os outros por ordem de chegada.
    pub fn members(&self) -> Vec<Member> {
        let me = self.room.local_participant();
        let mut list = vec![Member {
            id: me.identity().to_string(),
            is_me: true,
            speaking: me.is_speaking(),
            muted: self.mic.is_muted(),
            live: self.is_streaming(),
            camera: self.camera_on(),
        }];
        let mut others: Vec<RemoteParticipant> = self.room.remote_participants().into_values().collect();
        others.sort_by_key(|p| p.joined_at());
        for p in others {
            let pubs = p.track_publications();
            let mic = pubs.values().find(|t| t.source() == TrackSource::Microphone);
            list.push(Member {
                id: p.identity().to_string(),
                is_me: false,
                speaking: p.is_speaking(),
                muted: mic.is_none_or(|m| m.is_muted()),
                live: pubs.values().any(|t| t.source() == TrackSource::Screenshare),
                camera: pubs.values().any(|t| t.source() == TrackSource::Camera && !t.is_muted()),
            });
        }
        list
    }

    /// Ping (tempo de ida e volta até o servidor de voz), em ms. É o número que o Discord mostra.
    pub async fn ping_ms(&self) -> Option<u32> {
        use livekit::webrtc::stats::RtcStats;
        let stats = self.room.get_stats().await.ok()?;
        stats
            .publisher_stats
            .iter()
            .chain(stats.subscriber_stats.iter())
            .filter_map(|s| match s {
                RtcStats::CandidatePair(p) if p.candidate_pair.nominated && p.candidate_pair.current_round_trip_time > 0.0 => {
                    Some(p.candidate_pair.current_round_trip_time)
                }
                _ => None,
            })
            .reduce(f64::min)
            .map(|secs| (secs * 1000.0).round() as u32)
    }

    /// O moderador liberou a voz: publica o microfone de novo.
    /// Quando a permissão foi tirada, o servidor derrubou o microfone, mas o app ainda pode achar
    /// que ele está publicado; por isso tira a publicação velha e publica do zero. O LiveKit
    /// pode demorar um pouco para aplicar a permissão nova: tenta algumas vezes.
    pub async fn republish_mic(&self) {
        let me = self.room.local_participant();
        for tentativa in 0..6 {
            let old: Vec<_> = me
                .track_publications()
                .values()
                .filter(|t| t.source() == TrackSource::Microphone)
                .map(|t| t.sid())
                .collect();
            for sid in old {
                let _ = me.unpublish_track(&sid).await;
            }
            let ok = me
                .publish_track(
                    LocalTrack::Audio(self.mic.clone()),
                    TrackPublishOptions { source: TrackSource::Microphone, ..Default::default() },
                )
                .await
                .is_ok();
            if ok {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500 * (tentativa + 1))).await;
        }
    }

    pub fn set_muted(&self, muted: bool) {
        if muted {
            self.mic.mute();
        } else {
            self.mic.unmute();
        }
    }

    /// Ensurdecer = parar de tocar o áudio de todo mundo (inclusive o som das lives).
    pub fn set_deafened(&self, deafened: bool) {
        self.deafened.store(deafened, Ordering::SeqCst);
        let boosted = self.boosted.lock().unwrap();
        for p in self.room.remote_participants().values() {
            // A voz de quem tem volume próprio fica sempre desligada no WebRTC (toca pelo nosso tocador).
            let own = boosted.contains_key(p.identity().as_str());
            for publication in p.track_publications().values() {
                if let Some(RemoteTrack::Audio(track)) = publication.track() {
                    if deafened || (own && publication.source() == TrackSource::Microphone) {
                        track.disable();
                    } else {
                        track.enable();
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------ volume por pessoa

    /// O volume (ou o "silenciar") de uma pessoa mudou nas configurações: vale na hora.
    pub fn volume_changed(&self, identity: &str) {
        self.apply_volume(identity);
    }

    /// Microfone de uma pessoa, se já estiver chegando.
    fn mic_track(&self, identity: &str) -> Option<RemoteAudioTrack> {
        let p = self.room.remote_participants().into_values().find(|p| p.identity().as_str() == identity)?;
        let publication = p.track_publications().into_values().find(|t| t.source() == TrackSource::Microphone)?;
        match publication.track()? {
            RemoteTrack::Audio(track) => Some(track),
            _ => None,
        }
    }

    /// 100%: a voz toca pelo WebRTC, como sempre. Outro valor: toca pelo nosso tocador com ganho.
    fn apply_volume(&self, identity: &str) {
        let percent = crate::settings::get().volume_of(identity).min(200);
        let Some(track) = self.mic_track(identity) else {
            self.boosted.lock().unwrap().remove(identity);
            return;
        };
        let mut boosted = self.boosted.lock().unwrap();
        if percent == 100 {
            boosted.remove(identity);
            if !self.deafened.load(Ordering::SeqCst) {
                track.enable();
            }
            return;
        }
        let sid = track.sid().to_string();
        if let Some(b) = boosted.get(identity).filter(|b| b.sid == sid) {
            b.player.set_gain(percent);
            return;
        }
        let device = crate::settings::get().speaker.map(|d| d.id);
        let player = match Player::start(device, percent) {
            Ok(p) => Arc::new(p),
            Err(e) => {
                eprintln!("volume de {identity}: {e}");
                return;
            }
        };
        let mut stream = NativeAudioStream::new(track.rtc_track(), audio::SAMPLE_RATE as i32, audio::CHANNELS as i32);
        let (p, deafened) = (player.clone(), self.deafened.clone());
        let task = tokio::spawn(async move {
            while let Some(frame) = stream.next().await {
                if !deafened.load(Ordering::Relaxed) {
                    p.push(&frame.data);
                }
            }
        });
        boosted.insert(identity.to_string(), Boosted { sid, player, task });
        // Tira do mixer do WebRTC para não tocar duas vezes.
        track.disable();
    }

    /// A pessoa saiu da chamada: fecha o tocador dela.
    pub fn forget_player(&self, identity: &str) {
        self.boosted.lock().unwrap().remove(identity);
    }

    /// Sai da call. Se estava gravando, devolve o arquivo salvo.
    pub async fn leave(&self) -> Option<Recorded> {
        self.stop_camera().await;
        let recorded = self.stop_screen().await;
        self.stop_watching();
        self.boosted.lock().unwrap().clear();
        let _ = self.room.close().await;
        recorded
    }

    // ------------------------------------------------------------ assinaturas

    /// Decide o que baixar: microfones e câmeras sempre; tela + som da tela só de quem estou assistindo.
    /// Chamado ao entrar, quando alguém publica algo e quando troco de live.
    pub fn sync_subscriptions(&self) {
        let watching = self.watch.lock().unwrap().as_ref().map(|w| w.identity.clone());
        let hidden = crate::settings::get().hidden_cams;
        for p in self.room.remote_participants().values() {
            let watched = watching.as_deref() == Some(p.identity().as_str());
            for publication in p.track_publications().values() {
                let want = match publication.source() {
                    TrackSource::Microphone => true,
                    // "Desativar vídeo" de alguém: nem baixa a câmera dele.
                    TrackSource::Camera => !hidden.contains(p.identity().as_str()),
                    TrackSource::Screenshare | TrackSource::ScreenshareAudio => watched,
                    _ => false,
                };
                if publication.is_subscribed() != want {
                    publication.set_subscribed(want);
                }
                // Nos quadradinhos basta a versão média da câmera (menos internet para todo mundo).
                if publication.source() == TrackSource::Camera && want {
                    publication.set_video_quality(VideoQuality::Medium);
                }
            }
        }
    }

    // ------------------------------------------------------------ transmitir

    pub fn is_streaming(&self) -> bool {
        self.screen.lock().unwrap().is_some()
    }

    /// Começa a transmitir uma tela ou janela (`None` = tela principal).
    /// Devolve a resolução/encoder que ficou.
    pub async fn start_screen(&self, target: Option<Target>, height: u32, fps: u32, with_audio: bool) -> Result<StreamInfo> {
        if self.is_streaming() {
            bail!("você já está transmitindo");
        }
        // Liga GPU + captura + encoder (bloqueante: fora das threads do tokio).
        let cfg = StreamConfig { height, fps, target, ..Default::default() };
        let what = if matches!(target, Some(Target::Window(_))) { "a janela" } else { "a tela" };
        let mut stream = tokio::task::spawn_blocking(move || Stream::start(cfg))
            .await?
            .map_err(|e| anyhow!("não consegui capturar {what} ({e:#})"))?;
        let info = stream.info.clone();

        let lp = self.room.local_participant();
        let source = NativeVideoSource::new_encoded(VideoResolution { width: info.width, height: info.height });
        let track = LocalVideoTrack::create_video_track("screen", RtcVideoSource::Native(source.clone()));
        let publication = lp
            .publish_track(
                LocalTrack::Video(track),
                TrackPublishOptions {
                    source: TrackSource::Screenshare,
                    video_codec: VideoCodec::H264,
                    // O vídeo já vem codificado pela placa de vídeo: o LiveKit só empacota e envia.
                    video_encoder: VideoEncoderBackend::PreEncoded,
                    simulcast: false,
                    video_encoding: Some(VideoEncoding {
                        max_bitrate: info.bitrate as u64,
                        max_framerate: info.fps as f64,
                    }),
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| anyhow!("não consegui publicar a tela ({e})"))?;
        let mut tracks = vec![publication.sid()];
        stream.attach(Sink::LiveKit(source));

        // Som do PC (tudo menos o próprio Polley, pra voz da call não voltar como eco).
        let audio_stop = Arc::new(AtomicBool::new(false));
        let mut has_audio = false;
        if with_audio {
            // Janela: só o som do programa dela. Tela: o PC todo menos o Polley.
            let rx = match target.and_then(|t| t.process_id()) {
                Some(pid) => audio::start_app_audio(audio_stop.clone(), pid),
                None => audio::start_system_audio(audio_stop.clone()),
            };
            if let Ok(rx) = rx {
                let opts = AudioSourceOptions { echo_cancellation: false, noise_suppression: false, auto_gain_control: false };
                let src = NativeAudioSource::new(opts, audio::SAMPLE_RATE, audio::CHANNELS, 100);
                let track = LocalAudioTrack::create_audio_track("screen-audio", RtcAudioSource::Native(src.clone()));
                let options = TrackPublishOptions { source: TrackSource::ScreenshareAudio, ..Default::default() };
                if let Ok(publication) = lp.publish_track(LocalTrack::Audio(track), options).await {
                    tracks.push(publication.sid());
                    tokio::spawn(audio::pump_system_audio(rx, src, Some(stream.recording.clone())));
                    has_audio = true;
                }
            }
        }

        *self.screen.lock().unwrap() = Some(Screen { stream, tracks, audio_stop, with_audio: has_audio });
        Ok(info)
    }

    /// Para a live. Se estava gravando, fecha o arquivo e o devolve.
    pub async fn stop_screen(&self) -> Option<Recorded> {
        let screen = self.screen.lock().unwrap().take()?;
        let recording = screen.stream.recording.clone();
        let recorded = tokio::task::spawn_blocking(move || recording.stop()).await.ok().and_then(|r| r.ok()).flatten();
        screen.audio_stop.store(true, Ordering::Relaxed);
        let lp = self.room.local_participant();
        for sid in &screen.tracks {
            let _ = lp.unpublish_track(sid).await;
        }
        let stream = screen.stream;
        let _ = tokio::task::spawn_blocking(move || stream.stop()).await;
        recorded
    }

    // ------------------------------------------------------------ gravar

    /// Gravação da live atual (None = não estou transmitindo).
    pub fn recording(&self) -> Option<Arc<Recording>> {
        self.screen.lock().unwrap().as_ref().map(|s| s.stream.recording.clone())
    }

    /// Começa a gravar a live em `path` (.mp4), com o som do PC se a live tiver.
    pub fn start_recording(&self, path: std::path::PathBuf) -> Result<()> {
        let screen = self.screen.lock().unwrap();
        let screen = screen.as_ref().ok_or_else(|| anyhow!("comece a transmitir para gravar"))?;
        screen.stream.recording.start(path, screen.with_audio)
    }

    /// Se a live parou sozinha (a janela foi fechada, a placa falhou...), o motivo.
    pub fn screen_failure(&self) -> Option<String> {
        let screen = self.screen.lock().unwrap();
        let stream = &screen.as_ref()?.stream;
        if stream.is_running() {
            return None;
        }
        Some(stream.failure().unwrap_or_else(|| "a captura terminou".into()))
    }

    /// Quadros/bytes enviados até agora (para mostrar fps e Mbps).
    pub fn screen_counters(&self) -> Option<(u64, u64)> {
        let screen = self.screen.lock().unwrap();
        screen.as_ref().map(|s| (s.stream.frames.load(Ordering::Relaxed), s.stream.bytes.load(Ordering::Relaxed)))
    }

    // ------------------------------------------------------------ assistir

    pub fn watching(&self) -> Option<String> {
        self.watch.lock().unwrap().as_ref().map(|w| w.identity.clone())
    }

    pub fn watch(&self, identity: String, sink: FrameSink, gpu: Option<polley_media::videoview::VideoSink>) {
        self.watch_gen.fetch_add(1, Ordering::SeqCst);
        *self.watch.lock().unwrap() = Some(Watch { identity: identity.clone(), sink, gpu });
        self.frame_pending.store(false, Ordering::SeqCst);
        self.sync_subscriptions();
        // Se a trilha já estava assinada (ex.: voltou a assistir), liga o vídeo direto.
        if let Some(p) = self.room.remote_participants().values().find(|p| p.identity().as_str() == identity) {
            for publication in p.track_publications().values() {
                if let Some(RemoteTrack::Video(track)) = publication.track() {
                    if publication.source() == TrackSource::Screenshare {
                        self.play(track);
                    }
                }
            }
        }
    }

    pub fn stop_watching(&self) {
        self.watch_gen.fetch_add(1, Ordering::SeqCst);
        self.watch.lock().unwrap().take();
        self.sync_subscriptions();
    }

    /// Chamado pelo App quando uma trilha termina de ser assinada.
    pub fn on_track_subscribed(&self, track: RemoteTrack, publication: &RemoteTrackPublication, from: &RemoteParticipant) {
        if matches!(track, RemoteTrack::Audio(_)) && publication.source() == TrackSource::Microphone {
            return self.apply_volume(from.identity().as_str());
        }
        if let RemoteTrack::Video(video) = track {
            if publication.source() == TrackSource::Camera {
                return self.show_camera(from.identity().to_string(), video);
            }
            let watching = self.watching();
            if publication.source() == TrackSource::Screenshare && watching.as_deref() == Some(from.identity().as_str()) {
                self.play(video);
            }
        }
    }

    // ------------------------------------------------------------ câmera

    pub fn set_camera_sink(&self, sink: CameraSink) {
        *self.camera_sink.lock().unwrap() = Some(sink);
    }

    pub fn camera_on(&self) -> bool {
        self.cam.lock().unwrap().is_some()
    }

    /// Liga a minha câmera e publica na sala. A minha prévia vai para o mesmo `CameraSink`.
    pub async fn start_camera(&self) -> Result<CameraInfo> {
        if self.camera_on() {
            bail!("a câmera já está ligada");
        }
        let me = self.room.local_participant().identity().to_string();
        let sink = self.camera_sink.lock().unwrap().clone();
        let pending = Arc::new(AtomicBool::new(false));
        let view = self.cam_view.clone();
        let mut last = std::time::Instant::now() - CAMERA_FRAME_GAP;
        let bitrate = if crate::settings::get().camera_hq { camstream::BITRATE_HIGH } else { camstream::BITRATE_NORMAL };
        // Câmera + encoder na placa de vídeo (bloqueante: fora das threads do tokio).
        let mut stream = tokio::task::spawn_blocking(move || {
            let index = crate::settings::camera_index(&polley_media::camera::list());
            CameraStream::start(index, bitrate, move |frame| {
                // Prévia na minha tela, no tamanho do meu quadradinho.
                if let Some(sink) = &sink {
                    let (max, cover, gap) = view.layout(&me);
                    if !pending.load(Ordering::SeqCst) && last.elapsed() >= gap {
                        last = std::time::Instant::now();
                        if let Some(img) = to_rgba(frame, max, cover) {
                            pending.store(true, Ordering::SeqCst);
                            sink(me.clone(), img, pending.clone());
                        }
                    }
                }
            })
        })
        .await?
        .map_err(|e| anyhow!("não consegui ligar a câmera ({e:#})"))?;
        let info = stream.info.clone();
        let size = (info.width & !1, info.height & !1);

        // Já publicada nesta chamada com o mesmo tamanho: só tira a pausa.
        let reuse = self.cam_track.lock().unwrap().as_ref().filter(|t| t.size == size).map(|t| t.source.clone());
        if let Some(source) = reuse {
            stream.attach(source);
            if let Some(t) = self.cam_track.lock().unwrap().as_ref() {
                t.publication.unmute();
            }
            *self.cam.lock().unwrap() = Some(Cam { stream });
            return Ok(info);
        }
        // Trocou de câmera (tamanho diferente): tira a trilha antiga.
        let old = self.cam_track.lock().unwrap().take();
        if let Some(old) = old {
            let _ = self.room.local_participant().unpublish_track(&old.publication.sid()).await;
        }
        let source = NativeVideoSource::new_encoded(VideoResolution { width: size.0, height: size.1 });
        let track = LocalVideoTrack::create_video_track("camera", RtcVideoSource::Native(source.clone()));
        let publication = self
            .room
            .local_participant()
            .publish_track(
                LocalTrack::Video(track),
                TrackPublishOptions {
                    source: TrackSource::Camera,
                    video_codec: VideoCodec::H264,
                    // Já vem comprimida pela placa de vídeo: o LiveKit só empacota e envia.
                    video_encoder: VideoEncoderBackend::PreEncoded,
                    simulcast: false,
                    video_encoding: Some(VideoEncoding { max_bitrate: camstream::BITRATE_HIGH as u64, max_framerate: 30.0 }),
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| anyhow!("não consegui publicar a câmera ({e})"))?;
        stream.attach(source.clone());
        *self.cam_track.lock().unwrap() = Some(CamTrack { publication, source, size });
        *self.cam.lock().unwrap() = Some(Cam { stream });
        Ok(info)
    }

    pub async fn stop_camera(&self) {
        let Some(cam) = self.cam.lock().unwrap().take() else { return };
        // Pausa a trilha (os outros veem a foto de novo); ela fica publicada para religar.
        if let Some(t) = self.cam_track.lock().unwrap().as_ref() {
            t.publication.mute();
        }
        // Fecha câmera + encoder (a memória volta). Espera a thread: fora das threads do tokio.
        let stream = cam.stream;
        let _ = tokio::task::spawn_blocking(move || drop(stream)).await;
    }

    /// Câmera de alguém chegou: decodifica e manda para o quadradinho dela.
    fn show_camera(&self, identity: String, track: RemoteVideoTrack) {
        let Some(sink) = self.camera_sink.lock().unwrap().clone() else { return };
        let view = self.cam_view.clone();
        tokio::spawn(async move {
            let pending = Arc::new(AtomicBool::new(false));
            let mut last = std::time::Instant::now() - CAMERA_FRAME_GAP;
            let mut frames = NativeVideoStream::new(track.rtc_track());
            while let Some(frame) = frames.next().await {
                let (max, cover, gap) = view.layout(&identity);
                if pending.load(Ordering::SeqCst) || last.elapsed() < gap {
                    continue;
                }
                last = std::time::Instant::now();
                if let Some(img) = to_rgba(frame.buffer.as_ref(), max, cover) {
                    pending.store(true, Ordering::SeqCst);
                    sink(identity.clone(), img, pending.clone());
                }
            }
        });
    }

    /// Decodifica a live numa tarefa: cada quadro é reduzido para o tamanho da área da tela
    /// e convertido para RGBA. Se a interface estiver atrasada, o quadro é pulado (sem fila = sem atraso).
    fn play(&self, track: RemoteVideoTrack) {
        let Some((sink, gpu)) = self.watch.lock().unwrap().as_ref().map(|w| (w.sink.clone(), w.gpu.clone())) else { return };
        let current = self.watch_gen.clone();
        let gen = current.load(Ordering::SeqCst);
        let (viewport, pending) = (self.viewport.clone(), self.frame_pending.clone());
        let stats = self.watch_stats.clone();
        tokio::spawn(async move {
            // Fila de 1 quadro (padrão do SDK): se atrasar, o mais velho é descartado.
            let mut frames = NativeVideoStream::new(track.rtc_track());
            while let Some(frame) = frames.next().await {
                if current.load(Ordering::SeqCst) != gen {
                    break;
                }
                let buffer = &frame.buffer;
                let (fw, fh) = (buffer.width(), buffer.height());
                stats.received.fetch_add(1, Ordering::Relaxed);
                stats.width.store(fw, Ordering::Relaxed);
                stats.height.store(fh, Ordering::Relaxed);
                // Placa de vídeo: ela converte, redimensiona e desenha.
                if let Some(gpu) = &gpu {
                    if gpu.push(buffer.as_ref()) {
                        stats.shown.fetch_add(1, Ordering::Relaxed);
                    }
                    continue;
                }
                if pending.load(Ordering::SeqCst) {
                    continue;
                }
                let (vw, vh) = (viewport.0.load(Ordering::Relaxed).max(2), viewport.1.load(Ordering::Relaxed).max(2));
                // Cabe na área sem distorcer; nunca aumenta (aumentar só gastaria CPU).
                let scale = f64::min(vw as f64 / fw as f64, vh as f64 / fh as f64).min(1.0);
                let tw = (((fw as f64 * scale) as u32) & !1).max(2);
                let th = (((fh as f64 * scale) as u32) & !1).max(2);

                let mut i420 = if let Some(b) = buffer.as_i420() {
                    b.to_i420()
                } else if let Some(b) = buffer.as_native() {
                    b.to_i420()
                } else if let Some(b) = buffer.as_nv12() {
                    b.to_i420()
                } else {
                    continue;
                };
                let scaled = if (tw, th) != (fw, fh) { i420.scale(tw as i32, th as i32) } else { i420 };

                let mut image = SharedPixelBuffer::<Rgba8Pixel>::new(tw, th);
                // "ABGR" no libyuv = bytes R,G,B,A na memória = o RGBA que o Slint quer.
                scaled.to_argb(VideoFormatType::ABGR, image.make_mut_bytes(), tw * 4, tw as i32, th as i32);
                pending.store(true, Ordering::SeqCst);
                sink(image);
            }
        });
    }
}

#[derive(Default)]
pub struct WatchStats {
    pub received: AtomicU64,
    pub shown: AtomicU64,
    pub width: AtomicU32,
    pub height: AtomicU32,
}
