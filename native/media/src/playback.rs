//! Volume por pessoa na chamada.
//!
//! O módulo de áudio do WebRTC (que toca a voz de todo mundo) não deixa mudar o volume de UMA
//! pessoa. Então, quem tem volume diferente de 100% sai do mixer do WebRTC (a trilha fica em
//! volume 0 lá) e toca por aqui: um fluxo WASAPI próprio, com o ganho que a pessoa escolheu.
//! Quem está em 100% continua do jeito normal (sem custo extra).
//!
//! O som tocado aqui também não entra na captura do "som do PC" da live, porque ela ignora
//! o áudio do próprio Polley.

use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use windows::core::HSTRING;
use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::audio::{CHANNELS, SAMPLE_RATE};

/// Amostras por milissegundo (48 kHz, estéreo).
const PER_MS: usize = (SAMPLE_RATE / 1000 * CHANNELS) as usize;
/// Começa a tocar com 40 ms guardados (absorve a variação da internet).
const START_AT: usize = 40 * PER_MS;
/// Mais que 200 ms atrasado? Joga fora o excesso para não ficar com eco de atraso.
const MAX_QUEUE: usize = 200 * PER_MS;

pub struct Player {
    queue: Arc<Mutex<VecDeque<i16>>>,
    gain: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
}

impl Player {
    /// Abre a saída de áudio (a escolhida em "Voz e vídeo", ou a padrão do Windows).
    /// `gain` em porcentagem (100 = normal, 200 = o dobro).
    pub fn start(device_id: Option<String>, gain: u32) -> Result<Player> {
        let queue = Arc::new(Mutex::new(VecDeque::with_capacity(MAX_QUEUE)));
        let gain = Arc::new(AtomicU32::new(gain));
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
        {
            let (queue, gain, stop) = (queue.clone(), gain.clone(), stop.clone());
            std::thread::spawn(move || {
                if let Err(e) = unsafe { render(device_id, queue, gain, stop, &ready_tx) } {
                    let _ = ready_tx.send(Err(e));
                }
            });
        }
        ready_rx.recv().context("thread de áudio morreu")??;
        Ok(Player { queue, gain, stop })
    }

    pub fn set_gain(&self, percent: u32) {
        self.gain.store(percent, Ordering::Relaxed);
    }

    /// Recebe áudio já em 48 kHz estéreo.
    pub fn push(&self, samples: &[i16]) {
        let mut q = self.queue.lock().unwrap();
        q.extend(samples.iter().copied());
        if q.len() > MAX_QUEUE {
            let extra = q.len() - START_AT;
            q.drain(..extra - extra % CHANNELS as usize);
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

unsafe fn render(
    device_id: Option<String>,
    queue: Arc<Mutex<VecDeque<i16>>>,
    gain: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
    ready: &mpsc::Sender<Result<()>>,
) -> Result<()> {
    CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
    let devices: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
    let device = match device_id.filter(|id| !id.is_empty()) {
        Some(id) => devices.GetDevice(&HSTRING::from(id)).or_else(|_| devices.GetDefaultAudioEndpoint(eRender, eConsole))?,
        None => devices.GetDefaultAudioEndpoint(eRender, eConsole)?,
    };
    let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
    let block = (CHANNELS * 2) as u16;
    let format = WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM as u16,
        nChannels: CHANNELS as u16,
        nSamplesPerSec: SAMPLE_RATE,
        nAvgBytesPerSec: SAMPLE_RATE * block as u32,
        nBlockAlign: block,
        wBitsPerSample: 16,
        cbSize: 0,
    };
    client.Initialize(
        AUDCLNT_SHAREMODE_SHARED,
        AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
        400_000, // 40 ms
        0,
        &format,
        None,
    )?;
    let event = CreateEventW(None, false, false, None)?;
    client.SetEventHandle(event)?;
    let size = client.GetBufferSize()?;
    let out: IAudioRenderClient = client.GetService()?;
    client.Start()?;
    let _ = ready.send(Ok(()));

    let mut playing = false;
    while !stop.load(Ordering::Relaxed) {
        if WaitForSingleObject(event, 50) != WAIT_OBJECT_0 {
            continue;
        }
        let Ok(padding) = client.GetCurrentPadding() else { break };
        let frames = size.saturating_sub(padding);
        if frames == 0 {
            continue;
        }
        let Ok(ptr) = out.GetBuffer(frames) else { break };
        let buf = std::slice::from_raw_parts_mut(ptr as *mut i16, frames as usize * CHANNELS as usize);
        let g = gain.load(Ordering::Relaxed) as i32;
        {
            let mut q = queue.lock().unwrap();
            if !playing && q.len() >= START_AT {
                playing = true;
            }
            for s in buf.iter_mut() {
                *s = match playing.then(|| q.pop_front()).flatten() {
                    Some(v) => (v as i32 * g / 100).clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                    None => {
                        // Acabou o que tinha: espera juntar de novo antes de voltar a tocar.
                        playing = false;
                        0
                    }
                };
            }
        }
        let _ = out.ReleaseBuffer(frames, 0);
    }
    let _ = client.Stop();
    let _ = CloseHandle(event);
    Ok(())
}
