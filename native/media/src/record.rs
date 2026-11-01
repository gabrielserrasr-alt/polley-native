//! Gravação da live num arquivo .mp4.
//!
//! O vídeo NÃO é comprimido de novo: o arquivo recebe exatamente os bytes H.264 que a placa de vídeo
//! já gerou para a live (custo quase zero de CPU/RAM). O som do PC (PCM) vira AAC pelo próprio
//! Windows. Quem monta o .mp4 é o "Sink Writer" do Media Foundation.
//!
//! Fluxo: `start()` pede um keyframe → no primeiro keyframe o arquivo é criado (um .mp4 precisa
//! começar num quadro completo) → `video()`/`audio()` vão escrevendo → `stop()` fecha o arquivo.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use windows::core::HSTRING;
use windows::Win32::Media::MediaFoundation::*;

use crate::audio::{CHANNELS, SAMPLE_RATE};
use crate::encoder::EncodedFrame;

/// Som do PC no arquivo: AAC 192 kbps (24000 bytes/s é um dos valores que o encoder AAC do Windows aceita).
const AAC_BYTES_PER_SECOND: u32 = 24_000;

fn pack(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

/// Tira o SPS e o PPS (cabeçalho H.264) de um keyframe, com os códigos de início (formato Annex B).
fn sequence_header(data: &[u8]) -> Vec<u8> {
    // Posições de cada NAL (logo depois de 00 00 01).
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::new();
    for (k, &s) in starts.iter().enumerate() {
        let Some(&first) = data.get(s) else { continue };
        if matches!(first & 0x1f, 7 | 8) {
            let mut end = starts.get(k + 1).map(|&n| n - 3).unwrap_or(data.len());
            // O código de início de 4 bytes (00 00 00 01) deixa um zero sobrando no fim.
            while end > s && data[end - 1] == 0 {
                end -= 1;
            }
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(&data[s..end]);
        }
    }
    out
}

struct Writer {
    sink: IMFSinkWriter,
    video: u32,
    audio: Option<u32>,
    fps: u32,
    /// Tempo (µs) do primeiro quadro gravado: vira o zero do arquivo.
    first_us: i64,
    audio_samples: u64,
    path: PathBuf,
    started: Instant,
}

// O Sink Writer do Media Foundation pode ser usado de qualquer thread (e o Mutex garante uma por vez).
unsafe impl Send for Writer {}

impl Writer {
    fn create(path: &PathBuf, width: u32, height: u32, fps: u32, bitrate: u32, header: &[u8], audio: bool) -> Result<Self> {
        unsafe {
            let mut attrs = None;
            MFCreateAttributes(&mut attrs, 2)?;
            let attrs = attrs.context("atributos vazios")?;
            // Nunca segurar a live esperando o disco.
            attrs.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;
            attrs.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4)?;
            let sink = MFCreateSinkWriterFromURL(&HSTRING::from(path.as_os_str()), None, &attrs)
                .context("não consegui criar o arquivo")?;

            // Vídeo: H.264 que já vem pronto (entrada = saída, só empacota).
            let video_type = MFCreateMediaType()?;
            video_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            video_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            video_type.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            video_type.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
            video_type.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            video_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            video_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            video_type.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32)?;
            if !header.is_empty() {
                video_type.SetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, header)?;
            }
            let video = sink.AddStream(&video_type).context("o .mp4 recusou o vídeo")?;
            sink.SetInputMediaType(video, &video_type, None).context("o .mp4 recusou o vídeo")?;

            // Áudio: entra PCM 48 kHz estéreo, sai AAC.
            let audio = if audio {
                let block = CHANNELS * 2;
                let out = MFCreateMediaType()?;
                out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
                out.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
                out.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
                out.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, SAMPLE_RATE)?;
                out.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, CHANNELS)?;
                out.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, AAC_BYTES_PER_SECOND)?;
                let input = MFCreateMediaType()?;
                input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
                input.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
                input.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
                input.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, SAMPLE_RATE)?;
                input.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, CHANNELS)?;
                input.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, block)?;
                input.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, SAMPLE_RATE * block)?;
                input.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
                // Sem áudio no arquivo é melhor que sem arquivo: se o AAC falhar, grava só o vídeo.
                match sink.AddStream(&out).and_then(|i| sink.SetInputMediaType(i, &input, None).map(|_| i)) {
                    Ok(i) => Some(i),
                    Err(_) => None,
                }
            } else {
                None
            };

            sink.BeginWriting().context("não consegui começar a gravar")?;
            Ok(Self { sink, video, audio, fps, first_us: -1, audio_samples: 0, path: path.clone(), started: Instant::now() })
        }
    }

    fn write(&self, stream: u32, bytes: &[u8], time: i64, duration: i64, keyframe: bool) -> Result<()> {
        unsafe {
            let buffer = MFCreateMemoryBuffer(bytes.len() as u32)?;
            let mut ptr = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(bytes.len() as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(time)?;
            sample.SetSampleDuration(duration)?;
            if keyframe {
                sample.SetUINT32(&MFSampleExtension_CleanPoint, 1)?;
            }
            self.sink.WriteSample(stream, &sample)?;
        }
        Ok(())
    }

    fn video(&mut self, frame: &EncodedFrame) -> Result<()> {
        if self.first_us < 0 {
            self.first_us = frame.timestamp_us;
        }
        let time = (frame.timestamp_us - self.first_us) * 10; // µs → unidades de 100 ns
        self.write(self.video, frame.data, time, 10_000_000 / self.fps as i64, frame.keyframe)
    }

    fn audio(&mut self, pcm: &[i16]) -> Result<()> {
        let Some(stream) = self.audio else { return Ok(()) };
        let frames = (pcm.len() / CHANNELS as usize) as u64;
        let time = (self.audio_samples * 10_000_000 / SAMPLE_RATE as u64) as i64;
        let duration = (frames * 10_000_000 / SAMPLE_RATE as u64) as i64;
        self.audio_samples += frames;
        let bytes = unsafe { std::slice::from_raw_parts(pcm.as_ptr() as *const u8, std::mem::size_of_val(pcm)) };
        self.write(stream, bytes, time, duration, true)
    }
}

enum State {
    Off,
    /// Esperando o próximo keyframe para criar o arquivo.
    Waiting { path: PathBuf, audio: bool },
    On(Writer),
}

/// Controle da gravação de uma live (compartilhado entre a thread de vídeo, a de áudio e o app).
pub struct Recording {
    state: Mutex<State>,
    /// A thread de vídeo lê isto e pede um keyframe ao encoder.
    pub(crate) want_keyframe: AtomicBool,
    /// Erro que parou a gravação no meio (disco cheio etc.), para o app mostrar.
    error: Mutex<Option<String>>,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
}

/// Arquivo pronto.
pub struct Recorded {
    pub path: PathBuf,
    pub duration: Duration,
}

impl Recording {
    pub(crate) fn new(width: u32, height: u32, fps: u32, bitrate: u32) -> Self {
        Self {
            state: Mutex::new(State::Off),
            want_keyframe: AtomicBool::new(false),
            error: Mutex::new(None),
            width,
            height,
            fps,
            bitrate,
        }
    }

    /// Começa a gravar em `path` (.mp4). `audio`: incluir o som do PC.
    pub fn start(&self, path: PathBuf, audio: bool) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        if !matches!(*state, State::Off) {
            bail!("já está gravando");
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).context("não consegui criar a pasta da gravação")?;
        }
        *self.error.lock().unwrap() = None;
        *state = State::Waiting { path, audio };
        self.want_keyframe.store(true, Ordering::Relaxed);
        Ok(())
    }

    pub fn is_recording(&self) -> bool {
        !matches!(*self.state.lock().unwrap(), State::Off)
    }

    /// Tempo gravado até agora.
    pub fn elapsed(&self) -> Option<Duration> {
        match &*self.state.lock().unwrap() {
            State::On(w) => Some(w.started.elapsed()),
            State::Waiting { .. } => Some(Duration::ZERO),
            State::Off => None,
        }
    }

    /// Se a gravação parou sozinha por erro, devolve o motivo (uma vez).
    pub fn take_error(&self) -> Option<String> {
        self.error.lock().unwrap().take()
    }

    /// Para e fecha o arquivo (bloqueia até o .mp4 estar completo, normalmente < 1 s).
    pub fn stop(&self) -> Result<Option<Recorded>> {
        let state = std::mem::replace(&mut *self.state.lock().unwrap(), State::Off);
        let State::On(writer) = state else { return Ok(None) };
        let duration = writer.started.elapsed();
        unsafe { writer.sink.Finalize() }.context("não consegui fechar o arquivo")?;
        Ok(Some(Recorded { path: writer.path, duration }))
    }

    fn fail(&self, state: &mut State, e: anyhow::Error) {
        *state = State::Off;
        *self.error.lock().unwrap() = Some(format!("{e:#}"));
    }

    /// Chamado pela thread de vídeo a cada quadro codificado.
    pub(crate) fn video(&self, frame: &EncodedFrame) {
        let mut state = self.state.lock().unwrap();
        if let State::Waiting { path, audio } = &*state {
            if !frame.keyframe {
                return;
            }
            let (path, audio) = (path.clone(), *audio);
            let header = sequence_header(frame.data);
            match Writer::create(&path, self.width, self.height, self.fps, self.bitrate, &header, audio) {
                Ok(w) => *state = State::On(w),
                Err(e) => return self.fail(&mut state, e),
            }
        }
        if let State::On(w) = &mut *state {
            if let Err(e) = w.video(frame) {
                self.fail(&mut state, e);
            }
        }
    }

    /// Chamado com cada pedaço do som do PC (48 kHz, estéreo, i16).
    pub fn audio(&self, pcm: &[i16]) {
        let mut state = self.state.lock().unwrap();
        if let State::On(w) = &mut *state {
            if let Err(e) = w.audio(pcm) {
                self.fail(&mut state, e);
            }
        }
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
