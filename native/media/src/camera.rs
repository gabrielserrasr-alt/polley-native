//! Webcam com o Media Foundation (o mesmo do app Câmera do Windows).
//!
//! O Windows entrega os quadros já em NV12 (se a câmera manda MJPEG ou YUY2, o próprio leitor
//! converte). Cada quadro vai para quem chamou (o LiveKit comprime e envia; a interface mostra
//! uma prévia). A resolução escolhida é a melhor até 720p — mais que isso só gastaria internet
//! e CPU numa chamada.

use anyhow::{bail, Context, Result};
use livekit::webrtc::video_frame::NV12Buffer;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use windows::core::{Interface, PWSTR};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};

/// Maior altura que pedimos à câmera.
const MAX_HEIGHT: u32 = 720;

#[derive(Clone, Debug)]
pub struct CameraInfo {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

fn stream() -> u32 {
    MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32
}

unsafe fn devices() -> Result<Vec<IMFActivate>> {
    let mut attrs = None;
    MFCreateAttributes(&mut attrs, 1)?;
    let attrs = attrs.context("atributos")?;
    attrs.SetGUID(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID)?;
    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    MFEnumDeviceSources(&attrs, &mut list, &mut count)?;
    let mut out = Vec::new();
    for i in 0..count as usize {
        if let Some(a) = (*list.add(i)).take() {
            out.push(a);
        }
    }
    if !list.is_null() {
        CoTaskMemFree(Some(list as *const _));
    }
    Ok(out)
}

unsafe fn device_name(a: &IMFActivate) -> String {
    let mut name = PWSTR::null();
    let mut len = 0u32;
    if a.GetAllocatedString(&MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, &mut name, &mut len).is_err() {
        return "Câmera".into();
    }
    let s = name.to_string().unwrap_or_else(|_| "Câmera".into());
    CoTaskMemFree(Some(name.0 as *const _));
    s
}

/// Nomes das câmeras ligadas no PC.
pub fn list() -> Vec<String> {
    let run = || unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
        let names = devices()?.iter().map(|d| device_name(d)).collect();
        let _ = MFShutdown();
        Ok::<_, anyhow::Error>(names)
    };
    run().unwrap_or_default()
}

fn packed(t: &IMFMediaType, key: &windows::core::GUID) -> (u32, u32) {
    let v = unsafe { t.GetUINT64(key) }.unwrap_or(0);
    ((v >> 32) as u32, v as u32)
}

/// Formatos que a câmera `index` oferece: (largura, altura, fps, tipo). Para diagnóstico.
pub fn formats(index: usize) -> Vec<(u32, u32, u32, String)> {
    let run = || unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
        let list = devices()?;
        let device = list.get(index).context("câmera não encontrada")?;
        let source: IMFMediaSource = device.ActivateObject()?;
        let reader = MFCreateSourceReaderFromMediaSource(&source, None)?;
        let mut out = Vec::new();
        let mut i = 0;
        while let Ok(t) = reader.GetNativeMediaType(stream(), i) {
            i += 1;
            let (w, h) = packed(&t, &MF_MT_FRAME_SIZE);
            let (num, den) = packed(&t, &MF_MT_FRAME_RATE);
            let sub = t.GetGUID(&MF_MT_SUBTYPE).map(|g| {
                let b = g.data1.to_le_bytes();
                String::from_utf8_lossy(&b).to_string()
            });
            out.push((w, h, if den > 0 { num / den } else { 0 }, sub.unwrap_or_default()));
        }
        let _ = source.Shutdown();
        drop(reader);
        let _ = MFShutdown();
        Ok::<_, anyhow::Error>(out)
    };
    run().unwrap_or_default()
}

/// Escolhe o formato nativo: a maior altura até 720p e, empatando, o maior fps (até 30).
unsafe fn pick_format(reader: &IMFSourceReader) -> Result<IMFMediaType> {
    let mut best: Option<(IMFMediaType, (u32, u32))> = None;
    let mut i = 0;
    while let Ok(t) = reader.GetNativeMediaType(stream(), i) {
        i += 1;
        let (w, h) = packed(&t, &MF_MT_FRAME_SIZE);
        let (num, den) = packed(&t, &MF_MT_FRAME_RATE);
        let fps = if den > 0 { num / den } else { 0 };
        if w == 0 || h == 0 {
            continue;
        }
        // Acima de 720p só entra se não houver nada menor (fica com o menor de todos).
        let score = if h <= MAX_HEIGHT { (1, h * 100 + fps.min(30)) } else { (0, u32::MAX - h) };
        if best.as_ref().is_none_or(|(_, s)| score > *s) {
            best = Some((t, score));
        }
    }
    best.map(|(t, _)| t).context("a câmera não informou nenhum formato")
}

pub struct Camera {
    pub info: CameraInfo,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Camera {
    /// Liga a câmera `index` (0 = a primeira). `on_frame` roda na thread da câmera a cada quadro.
    /// Bloqueia até a câmera abrir (ou falhar).
    pub fn start(index: usize, on_frame: impl FnMut(NV12Buffer) + Send + 'static) -> Result<Camera> {
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::channel::<Result<CameraInfo>>();
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new().name("polley-camera".into()).spawn(move || {
                if let Err(e) = run(index, stop, on_frame, &ready_tx) {
                    let _ = ready_tx.send(Err(e));
                }
            })?
        };
        match ready_rx.recv() {
            Ok(Ok(info)) => Ok(Camera { info, stop, thread: Some(thread) }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => bail!("a câmera parou sem explicar"),
        }
    }

    pub fn is_running(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }
}

impl Drop for Camera {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(
    index: usize,
    stop: Arc<AtomicBool>,
    mut on_frame: impl FnMut(NV12Buffer),
    ready: &mpsc::Sender<Result<CameraInfo>>,
) -> Result<()> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
        let result = (|| -> Result<()> {
            let list = devices()?;
            let device = list.get(index).or(list.first()).context("nenhuma câmera encontrada")?;
            let name = device_name(device);
            let source: IMFMediaSource = device
                .ActivateObject()
                .context("a câmera está sendo usada por outro programa ou o Windows bloqueou o acesso")?;

            // Deixa o leitor converter MJPEG/YUY2 → NV12 sozinho.
            let mut attrs = None;
            MFCreateAttributes(&mut attrs, 1)?;
            let attrs = attrs.context("atributos")?;
            attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)?;
            let reader = MFCreateSourceReaderFromMediaSource(&source, &attrs)?;

            let native = pick_format(&reader)?;
            reader.SetCurrentMediaType(stream(), None, &native)?;
            let (w, h) = packed(&native, &MF_MT_FRAME_SIZE);
            let (num, den) = packed(&native, &MF_MT_FRAME_RATE);
            let fps = if den > 0 { num / den } else { 30 };

            let out = MFCreateMediaType()?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            out.SetUINT64(&MF_MT_FRAME_SIZE, ((w as u64) << 32) | h as u64)?;
            reader.SetCurrentMediaType(stream(), None, &out).context("a câmera não aceitou o formato NV12")?;

            let _ = ready.send(Ok(CameraInfo { name, width: w, height: h, fps }));

            let (w, h) = (w & !1, h & !1);
            while !stop.load(Ordering::Relaxed) {
                let mut flags = 0u32;
                let mut sample = None;
                reader.ReadSample(stream(), 0, None, Some(&mut flags), None, Some(&mut sample))?;
                if flags & (MF_SOURCE_READERF_ERROR.0 as u32 | MF_SOURCE_READERF_ENDOFSTREAM.0 as u32) != 0 {
                    bail!("a câmera foi desconectada");
                }
                let Some(sample) = sample else { continue };
                let buffer = sample.ConvertToContiguousBuffer()?;
                let mut frame = NV12Buffer::new(w, h);
                copy_nv12(&buffer, w, h, &mut frame)?;
                on_frame(frame);
            }
            let _ = source.Shutdown();
            Ok(())
        })();
        let _ = MFShutdown();
        result
    }
}

/// Copia um quadro NV12 do buffer do Windows (que pode ter linhas maiores que a imagem) para o
/// buffer do WebRTC.
unsafe fn copy_nv12(buffer: &IMFMediaBuffer, w: u32, h: u32, dst: &mut NV12Buffer) -> Result<()> {
    let (w, h) = (w as usize, h as usize);
    let (dst_sy, dst_suv) = dst.strides();
    let (dst_y, dst_uv) = dst.data_mut();

    // Com IMF2DBuffer sabemos o tamanho real de cada linha ("pitch").
    if let Ok(b2) = buffer.cast::<IMF2DBuffer>() {
        let mut ptr = std::ptr::null_mut();
        let mut pitch = 0i32;
        b2.Lock2D(&mut ptr, &mut pitch)?;
        let total = b2.GetContiguousLength().unwrap_or(0) as usize;
        let pitch_abs = pitch.unsigned_abs() as usize;
        // Linhas do plano Y (pode ter sobra de alinhamento): total = pitch * linhas * 3/2.
        let y_rows = if pitch_abs > 0 && total > 0 { (total / pitch_abs) * 2 / 3 } else { h };
        let ok = pitch > 0 && pitch_abs >= w && y_rows >= h;
        if ok {
            let src = std::slice::from_raw_parts(ptr, pitch_abs * y_rows * 3 / 2);
            copy_planes(src, pitch_abs, y_rows, w, h, dst_y, dst_sy as usize, dst_uv, dst_suv as usize);
        }
        b2.Unlock2D()?;
        if ok {
            return Ok(());
        }
    }

    let mut ptr = std::ptr::null_mut();
    let mut len = 0u32;
    buffer.Lock(&mut ptr, None, Some(&mut len))?;
    let len = len as usize;
    let result = if len >= w * h * 3 / 2 {
        let src = std::slice::from_raw_parts(ptr, len);
        copy_planes(src, w, h, w, h, dst_y, dst_sy as usize, dst_uv, dst_suv as usize);
        Ok(())
    } else {
        Err(anyhow::anyhow!("quadro da câmera menor que o esperado"))
    };
    buffer.Unlock()?;
    result
}

#[allow(clippy::too_many_arguments)]
fn copy_planes(src: &[u8], pitch: usize, y_rows: usize, w: usize, h: usize, dst_y: &mut [u8], sy: usize, dst_uv: &mut [u8], suv: usize) {
    for row in 0..h {
        dst_y[row * sy..row * sy + w].copy_from_slice(&src[row * pitch..row * pitch + w]);
    }
    let uv = &src[pitch * y_rows..];
    for row in 0..h / 2 {
        dst_uv[row * suv..row * suv + w].copy_from_slice(&uv[row * pitch..row * pitch + w]);
    }
}
