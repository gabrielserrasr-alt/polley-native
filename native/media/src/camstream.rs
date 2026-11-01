//! Câmera na chamada com o NOSSO compressor (o mesmo da transmissão de tela).
//!
//! Antes a câmera ia crua para o WebRTC, que comprimia em 3 qualidades com o compressor dele
//! (VP8, na CPU): ~55 MB que ele só soltava ao sair da chamada. Agora:
//!   câmera (NV12 na RAM) → textura na placa de vídeo → encoder H.264 da placa → LiveKit
//! e ao desligar a câmera tudo isso é fechado: a memória volta na hora.

use crate::camera::{Camera, CameraInfo};
use crate::encoder::EncodedFrame;
use crate::gpu;
use crate::stream::open_encoder;
use livekit::webrtc::prelude::VideoBuffer;
use anyhow::{bail, Context, Result};
use livekit::webrtc::video_frame::{EncodedFrameType, EncodedVideoCodec, EncodedVideoFrame, NV12Buffer};
use livekit::webrtc::video_source::native::NativeVideoSource;
use livekit::webrtc::video_source::VideoResolution;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;

/// Bitrate da câmera: "Normal" (internet fraca aguenta) e "Alta" (mais nítida ao se mexer).
pub const BITRATE_NORMAL: u32 = 1_500_000;
pub const BITRATE_HIGH: u32 = 2_500_000;
/// Texturas em rodízio na placa de vídeo (o encoder pode estar lendo uma enquanto escrevemos).
const RING: usize = 3;

pub struct CameraStream {
    pub info: CameraInfo,
    pub encoder: String,
    stop: Arc<AtomicBool>,
    sink_tx: Option<mpsc::Sender<NativeVideoSource>>,
    thread: Option<JoinHandle<()>>,
}

impl CameraStream {
    /// Liga a câmera `index` e o encoder. `preview` recebe cada quadro (para a prévia na tela).
    /// Bloqueia até estar tudo pronto (ou falhar). Os quadros só vão para a chamada depois de
    /// [`CameraStream::attach`].
    pub fn start(index: usize, bitrate: u32, preview: impl FnMut(&NV12Buffer) + Send + 'static) -> Result<CameraStream> {
        let stop = Arc::new(AtomicBool::new(false));
        let (setup_tx, setup_rx) = mpsc::channel::<Result<(CameraInfo, String)>>();
        let (sink_tx, sink_rx) = mpsc::channel::<NativeVideoSource>();
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new().name("polley-camera-encoder".into()).spawn(move || {
                if let Err(e) = run(index, bitrate, stop, preview, &setup_tx, sink_rx) {
                    let _ = setup_tx.send(Err(e));
                }
            })?
        };
        match setup_rx.recv() {
            Ok(Ok((info, encoder))) => Ok(CameraStream { info, encoder, stop, sink_tx: Some(sink_tx), thread: Some(thread) }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => bail!("a câmera parou sem explicar"),
        }
    }

    /// Começa a mandar para a chamada.
    pub fn attach(&mut self, source: NativeVideoSource) {
        if let Some(tx) = &self.sink_tx {
            let _ = tx.send(source);
        }
    }
}

impl Drop for CameraStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.sink_tx.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(
    index: usize,
    bitrate: u32,
    stop: Arc<AtomicBool>,
    mut preview: impl FnMut(&NV12Buffer) + Send + 'static,
    setup_tx: &mpsc::Sender<Result<(CameraInfo, String)>>,
    sink_rx: mpsc::Receiver<NativeVideoSource>,
) -> Result<()> {
    unsafe {
        windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED).ok()?;
    }
    // O último quadro da câmera (se o encoder não pegou a tempo, o próximo substitui).
    let latest: Arc<Mutex<Option<NV12Buffer>>> = Arc::default();
    let camera = {
        let latest = latest.clone();
        Camera::start(index, move |frame| {
            preview(&frame);
            *latest.lock().unwrap() = Some(frame);
        })?
    };
    let info = camera.info.clone();
    let (w, h) = (info.width & !1, info.height & !1);
    let fps = info.fps.clamp(15, 30);

    // Primeira placa de vídeo cujo encoder abrir (NVIDIA/AMD primeiro).
    let mut last_err = anyhow::anyhow!("nenhuma placa de vídeo encontrada");
    let mut opened = None;
    for adapter in gpu::adapters()? {
        match gpu::Gpu::new(&adapter).and_then(|g| {
            let enc = open_encoder(&g, w, h, fps, bitrate, false)?;
            Ok((g, enc))
        }) {
            Ok(v) => {
                opened = Some(v);
                break;
            }
            Err(e) => last_err = e,
        }
    }
    let Some((gpu, mut enc)) = opened else { return Err(last_err).context("encoder da câmera") };
    let ring: Vec<ID3D11Texture2D> = (0..RING).map(|_| gpu.texture(w, h, DXGI_FORMAT_NV12)).collect::<Result<_>>()?;
    let _ = setup_tx.send(Ok((info, enc.name().to_string())));

    let Ok(source) = sink_rx.recv() else { return Ok(()) };
    let control = enc.control();
    // Quadro contínuo (Y e depois UV, sem sobras): é o que o UpdateSubresource espera no NV12.
    let mut packed = vec![0u8; (w * h * 3 / 2) as usize];
    let frame_time = Duration::from_secs_f64(1.0 / fps as f64);
    let start = Instant::now();
    let mut has_frame = false;
    let result = enc.run(
        &stop,
        |n| {
            let due = start + frame_time * n as u32;
            if let Some(wait) = due.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
            if !camera.is_running() {
                bail!("a câmera foi desconectada");
            }
            if source.take_keyframe_request() {
                control.force_keyframe();
            }
            if let Some(rc) = source.take_rate_control_request() {
                control.set_bitrate((rc.target_bitrate_bps as u32).clamp(300_000, bitrate));
            }
            // Sem quadro novo (câmera em 15 fps, luz baixa): repete o anterior.
            if let Some(frame) = latest.lock().unwrap().take() {
                pack(&frame, w, h, &mut packed);
                has_frame = true;
            }
            let tex = &ring[n as usize % RING];
            if has_frame {
                unsafe { gpu.context.UpdateSubresource(tex, 0, None, packed.as_ptr() as *const _, w, 0) };
            }
            Ok(tex.clone())
        },
        |f: EncodedFrame| {
            source.capture_encoded_frame(&EncodedVideoFrame {
                codec: EncodedVideoCodec::H264,
                payload: f.data,
                timestamp_us: f.timestamp_us,
                frame_type: if f.keyframe { EncodedFrameType::Key } else { EncodedFrameType::Delta },
                resolution: VideoResolution { width: w, height: h },
                frame_metadata: None,
            });
            Ok(())
        },
    );
    drop(enc);
    drop(camera);
    result
}

/// Copia o NV12 do WebRTC (linhas com sobra) para um bloco contínuo de `w`×`h`.
fn pack(frame: &NV12Buffer, w: u32, h: u32, out: &mut [u8]) {
    let (w, h) = (w as usize, h as usize);
    let (sy, suv) = frame.strides();
    let (y, uv) = frame.data();
    let (sy, suv) = (sy as usize, suv as usize);
    let fw = (frame.width() as usize).min(w);
    for row in 0..h.min(frame.height() as usize) {
        out[row * w..row * w + fw].copy_from_slice(&y[row * sy..row * sy + fw]);
    }
    let uv_out = &mut out[w * h..];
    for row in 0..(h / 2).min(frame.height() as usize / 2) {
        uv_out[row * w..row * w + fw].copy_from_slice(&uv[row * suv..row * suv + fw]);
    }
}
