//! Transmissão de tela pronta para usar: liga captura → conversão → encoder numa thread própria
//! e entrega os quadros H.264 para o LiveKit (ou para um arquivo).
//!
//! Uso:
//!   let s = Stream::start(StreamConfig { height: 1080, fps: 60, ..Default::default() })?; // bloqueia ~0,3 s
//!   let source = NativeVideoSource::new_encoded(VideoResolution { width: s.info.width, height: s.info.height });
//!   ... publicar a trilha ...
//!   s.attach(Sink::LiveKit(source));
//!   ...
//!   s.stop(); // bloqueia até a thread terminar

use crate::record::Recording;
use crate::{capture, convert, encoder, gpu, nvenc};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use anyhow::{bail, Context, Result};
use livekit::webrtc::video_frame::{EncodedFrameType, EncodedVideoCodec, EncodedVideoFrame};
use livekit::webrtc::video_source::native::NativeVideoSource;
use livekit::webrtc::video_source::VideoResolution;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Bitrate por resolução — mesmos valores do app Electron (legado/electron/src/renderer/src/lib/capture.ts).
pub fn bitrate_for(height: u32, fps: u32) -> u32 {
    let hi = fps > 30;
    match height {
        h if h > 1440 => if hi { 18_000_000 } else { 14_000_000 },
        h if h > 1080 => if hi { 10_000_000 } else { 8_000_000 },
        h if h > 720 => if hi { 6_000_000 } else { 4_500_000 },
        _ => if hi { 3_500_000 } else { 2_500_000 },
    }
}

/// Resolução de saída: nunca maior que a tela, mantendo a proporção, sempre par (exigência do H.264).
pub fn output_size(src_w: u32, src_h: u32, target_h: u32, force: bool) -> (u32, u32) {
    let h = if force { target_h } else { target_h.min(src_h) };
    let w = (src_w as u64 * h as u64 / src_h as u64) as u32;
    (w & !1, h & !1)
}

#[derive(Clone)]
pub struct StreamConfig {
    /// Altura desejada: 720, 1080, 1440 ou 2160.
    pub height: u32,
    pub fps: u32,
    /// Força a resolução mesmo que a tela seja menor (para medir o pior caso).
    pub force: bool,
    /// Parte do nome da placa de vídeo a usar (ex.: "intel"). `None` = tenta todas.
    pub gpu: Option<String>,
    /// Escreve no terminal qual placa serviu / falhou.
    pub verbose: bool,
    /// O que transmitir. `None` = monitor principal.
    pub target: Option<capture::Target>,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self { height: 1080, fps: 60, force: false, gpu: None, verbose: false, target: None }
    }
}

#[derive(Clone, Debug)]
pub struct StreamInfo {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
    pub encoder: String,
    pub gpu: String,
}

/// Para onde vão os quadros codificados.
pub enum Sink {
    LiveKit(NativeVideoSource),
    File(std::path::PathBuf),
}

pub struct Stream {
    pub info: StreamInfo,
    /// Contadores (para mostrar fps/Mbps).
    pub frames: Arc<AtomicU64>,
    pub bytes: Arc<AtomicU64>,
    /// Gravar a live num .mp4 (liga/desliga a qualquer momento).
    pub recording: Arc<Recording>,
    stop: Arc<AtomicBool>,
    /// Motivo, se a transmissão parou sozinha (ex.: a janela foi fechada).
    failure: Arc<Mutex<Option<String>>>,
    sink_tx: Option<mpsc::Sender<Sink>>,
    thread: Option<JoinHandle<Result<()>>>,
}

impl Stream {
    /// Liga a GPU, a captura e o encoder. Bloqueia até estar tudo pronto (ou falhar).
    /// A transmissão só começa de fato depois de [`Stream::attach`].
    pub fn start(cfg: StreamConfig) -> Result<Stream> {
        let stop = Arc::new(AtomicBool::new(false));
        let frames = Arc::new(AtomicU64::new(0));
        let bytes = Arc::new(AtomicU64::new(0));
        let (setup_tx, setup_rx) = mpsc::channel::<Result<(StreamInfo, Arc<Recording>)>>();
        let (sink_tx, sink_rx) = mpsc::channel::<Sink>();

        let failure = Arc::new(Mutex::new(None));
        let thread = {
            let (stop, frames, bytes, failure) = (stop.clone(), frames.clone(), bytes.clone(), failure.clone());
            std::thread::Builder::new().name("polley-encoder".into()).spawn(move || {
                let result = run(cfg, stop, frames, bytes, setup_tx, sink_rx);
                if let Err(e) = &result {
                    *failure.lock().unwrap() = Some(format!("{e:#}"));
                }
                result
            })?
        };

        match setup_rx.recv() {
            Ok(Ok((info, recording))) => {
                Ok(Stream { info, frames, bytes, recording, stop, failure, sink_tx: Some(sink_tx), thread: Some(thread) })
            }
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => match thread.join() {
                Ok(Err(e)) => Err(e),
                _ => bail!("a thread de vídeo parou sem explicar"),
            },
        }
    }

    /// Começa a mandar quadros para o destino.
    pub fn attach(&mut self, sink: Sink) {
        if let Some(tx) = self.sink_tx.take() {
            let _ = tx.send(sink);
        }
    }

    pub fn is_running(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }

    /// Se a transmissão parou sozinha, por quê.
    pub fn failure(&self) -> Option<String> {
        self.failure.lock().unwrap().clone()
    }

    /// Para e espera a thread terminar. Devolve o erro, se ela tiver morrido por um.
    pub fn stop(mut self) -> Result<()> {
        self.shutdown()
    }

    fn shutdown(&mut self) -> Result<()> {
        self.stop.store(true, Ordering::Relaxed);
        self.sink_tx.take(); // destrava quem ainda esperava o destino
        match self.thread.take().map(|t| t.join()) {
            Some(Ok(r)) => r,
            Some(Err(_)) => bail!("a thread de vídeo travou"),
            None => Ok(()),
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

/// O encoder escolhido para a placa: NVENC direto nas NVIDIA, Media Foundation nas outras.
pub(crate) enum AnyEncoder {
    Nvenc(nvenc::Nvenc),
    MediaFoundation(encoder::Encoder),
}

impl AnyEncoder {
    pub(crate) fn name(&self) -> &str {
        match self {
            Self::Nvenc(e) => &e.name,
            Self::MediaFoundation(e) => &e.name,
        }
    }

    pub(crate) fn control(&self) -> encoder::EncoderControl {
        match self {
            Self::Nvenc(e) => e.control(),
            Self::MediaFoundation(e) => e.control(),
        }
    }

    pub(crate) fn run(
        &mut self,
        stop: &AtomicBool,
        next_input: impl FnMut(u64) -> Result<ID3D11Texture2D>,
        on_output: impl FnMut(encoder::EncodedFrame) -> Result<()>,
    ) -> Result<()> {
        match self {
            Self::Nvenc(e) => e.run(stop, next_input, on_output),
            Self::MediaFoundation(e) => e.run(stop, next_input, on_output),
        }
    }
}

pub(crate) fn open_encoder(gpu: &gpu::Gpu, w: u32, h: u32, fps: u32, bitrate: u32, verbose: bool) -> Result<AnyEncoder> {
    // Na NVIDIA o encoder pelo Media Foundation costuma recusar abrir em notebooks com 2 placas;
    // direto no driver funciona (é o que o OBS faz).
    if gpu.name.contains("NVIDIA") {
        match nvenc::Nvenc::new(&gpu.device, w, h, fps, bitrate) {
            Ok(e) => return Ok(AnyEncoder::Nvenc(e)),
            Err(e) if verbose => println!("  NVENC direto não abriu: {e:#}; tentando o Media Foundation"),
            Err(_) => {}
        }
    }
    Ok(AnyEncoder::MediaFoundation(encoder::Encoder::new(&gpu.device, gpu.luid, w, h, fps, bitrate)?))
}

fn run(
    cfg: StreamConfig,
    stop: Arc<AtomicBool>,
    frames: Arc<AtomicU64>,
    bytes: Arc<AtomicU64>,
    setup_tx: mpsc::Sender<Result<(StreamInfo, Arc<Recording>)>>,
    sink_rx: mpsc::Receiver<Sink>,
) -> Result<()> {
    // O Media Foundation é uma API COM bloqueante: roda nesta thread só dela.
    unsafe {
        windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED).ok()?;
        windows::Win32::Media::MediaFoundation::MFStartup(
            windows::Win32::Media::MediaFoundation::MF_VERSION,
            windows::Win32::Media::MediaFoundation::MFSTARTUP_FULL,
        )?;
    }
    let fps = cfg.fps;

    // Tenta cada placa de vídeo até uma funcionar (NVIDIA/AMD primeiro).
    let try_adapter = |adapter: &gpu::Adapter| -> Result<_> {
        let gpu = gpu::Gpu::new(adapter).context("etapa GPU")?;
        let cap = match cfg.target {
            Some(t) => capture::ScreenCapture::new(&gpu, t),
            None => capture::ScreenCapture::primary_monitor(&gpu),
        }
        .context("etapa captura")?;
        let (w, h) = output_size(cap.width, cap.height, cfg.height, cfg.force);
        let bitrate = bitrate_for(h, fps);
        let conv = convert::Converter::new(&gpu, cap.buffer_width, cap.buffer_height, w, h, fps).context("etapa conversão")?;
        let enc = open_encoder(&gpu, w, h, fps, bitrate, cfg.verbose).context("etapa encoder")?;
        if cfg.verbose {
            println!("Placa: {} | tela {}x{} -> saída {w}x{h}", gpu.name, cap.width, cap.height);
        }
        Ok((gpu, cap, conv, enc, w, h, bitrate))
    };
    let setup = (|| -> Result<_> {
        let mut last_err = anyhow::anyhow!("nenhuma placa de vídeo encontrada");
        for adapter in gpu::adapters()? {
            if let Some(pref) = &cfg.gpu {
                if !adapter.name.to_lowercase().contains(&pref.to_lowercase()) {
                    continue;
                }
            }
            match try_adapter(&adapter) {
                Ok(v) => return Ok(v),
                Err(e) => {
                    if cfg.verbose {
                        println!("  {} não serviu: {e:#}", adapter.name);
                    }
                    last_err = e;
                }
            }
        }
        Err(last_err)
    })();
    let (gpu, cap, conv, mut enc, w, h, bitrate) = match setup {
        Ok(v) => v,
        Err(e) => {
            let msg = format!("{e:#}");
            let _ = setup_tx.send(Err(e));
            bail!(msg);
        }
    };
    let recording = Arc::new(Recording::new(w, h, fps, bitrate));
    let info = StreamInfo { width: w, height: h, fps, bitrate, encoder: enc.name().to_string(), gpu: gpu.name.clone() };
    let _ = setup_tx.send(Ok((info, recording.clone())));

    // Espera o destino (a trilha do LiveKit precisa ser publicada antes).
    let Ok(sink) = sink_rx.recv() else { return Ok(()) };
    let (source, mut file) = match sink {
        Sink::LiveKit(s) => (Some(s), None),
        Sink::File(p) => (None, Some(std::io::BufWriter::new(std::fs::File::create(p)?))),
    };
    let control = enc.control();

    // Espera o primeiro quadro da tela.
    let t0 = Instant::now();
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        if let Some((tex, fw, fh)) = cap.latest()? {
            conv.update_source(&tex, fw, fh);
            break;
        }
        if cap.is_closed() {
            bail!("a janela que você estava transmitindo foi fechada");
        }
        if t0.elapsed() > Duration::from_secs(5) {
            bail!("a captura de tela não entregou nenhum quadro");
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    let frame_time = Duration::from_secs_f64(1.0 / fps as f64);
    let start = Instant::now();
    enc.run(
        &stop,
        |n| {
            // Ritmo: um quadro a cada 1/fps segundos.
            let due = start + frame_time * n as u32;
            if let Some(wait) = due.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
            // Gravação começando: o .mp4 precisa começar num keyframe.
            if recording.want_keyframe.swap(false, Ordering::Relaxed) {
                control.force_keyframe();
            }
            // Pedidos do LiveKit: keyframe (alguém entrou) e ajuste de bitrate (rede).
            if let Some(s) = &source {
                if s.take_keyframe_request() {
                    control.force_keyframe();
                }
                if let Some(rc) = s.take_rate_control_request() {
                    control.set_bitrate((rc.target_bitrate_bps as u32).clamp(500_000, bitrate));
                }
            }
            if let Some((tex, fw, fh)) = cap.latest()? {
                conv.update_source(&tex, fw, fh);
            } else if cap.is_closed() {
                bail!("a janela que você estava transmitindo foi fechada");
            }
            conv.convert(n as usize)
        },
        |f| {
            bytes.fetch_add(f.data.len() as u64, Ordering::Relaxed);
            frames.fetch_add(1, Ordering::Relaxed);
            recording.video(&f);
            if let Some(file) = file.as_mut() {
                file.write_all(f.data)?;
            }
            if let Some(s) = &source {
                s.capture_encoded_frame(&EncodedVideoFrame {
                    codec: EncodedVideoCodec::H264,
                    payload: f.data,
                    timestamp_us: f.timestamp_us,
                    frame_type: if f.keyframe { EncodedFrameType::Key } else { EncodedFrameType::Delta },
                    resolution: VideoResolution { width: w, height: h },
                    frame_metadata: None,
                });
            }
            Ok(())
        },
    )?;
    if let Some(mut f) = file {
        f.flush()?;
    }
    Ok(())
}
