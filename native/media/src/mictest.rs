//! "Testar microfone" (Configurações → Voz e vídeo).
//!
//! Lê o microfone direto do Windows (WASAPI), mede o volume e, se a pessoa quiser, toca de volta
//! no fone para ela se ouvir. Com caixa de som, esse retorno vira microfonia (o som sai, entra no
//! microfone, sai mais alto...): quando o volume fica no máximo por um tempo, o retorno desliga
//! sozinho e o app avisa.

use anyhow::{Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};
use windows::core::HSTRING;
use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::audio::{CHANNELS, SAMPLE_RATE};
use crate::playback::Player;

/// O que o teste avisa para a interface.
pub enum Event {
    /// Volume de 0.0 a 1.0 (cerca de 20 vezes por segundo).
    Level(f32),
    /// Microfonia: o retorno foi desligado.
    Feedback,
}

pub struct MicTest {
    stop: Arc<AtomicBool>,
    monitor: Arc<AtomicBool>,
}

impl MicTest {
    /// `mic`/`speaker`: ids do Windows escolhidos em "Voz e vídeo" (None = padrão).
    pub fn start(
        mic: Option<String>,
        speaker: Option<String>,
        on_event: impl Fn(Event) + Send + 'static,
    ) -> Result<MicTest> {
        let stop = Arc::new(AtomicBool::new(false));
        let monitor = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
        {
            let (stop, monitor) = (stop.clone(), monitor.clone());
            std::thread::spawn(move || {
                if let Err(e) = unsafe { run(mic, speaker, stop, monitor, &on_event, &ready_tx) } {
                    let _ = ready_tx.send(Err(e));
                }
            });
        }
        ready_rx.recv().context("thread do microfone morreu")??;
        Ok(MicTest { stop, monitor })
    }

    /// Ouvir o próprio microfone no fone.
    pub fn set_monitor(&self, on: bool) {
        self.monitor.store(on, Ordering::Relaxed);
    }
}

impl Drop for MicTest {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

unsafe fn run(
    mic: Option<String>,
    speaker: Option<String>,
    stop: Arc<AtomicBool>,
    monitor: Arc<AtomicBool>,
    on_event: &dyn Fn(Event),
    ready: &mpsc::Sender<Result<()>>,
) -> Result<()> {
    CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
    let devices: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
    let device = match mic.filter(|id| !id.is_empty()) {
        Some(id) => devices.GetDevice(&HSTRING::from(id)).or_else(|_| devices.GetDefaultAudioEndpoint(eCapture, eCommunications))?,
        None => devices.GetDefaultAudioEndpoint(eCapture, eCommunications)?,
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
    client
        .Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            200_000, // 20 ms
            0,
            &format,
            None,
        )
        .context("não consegui abrir o microfone")?;
    let event = CreateEventW(None, false, false, None)?;
    client.SetEventHandle(event)?;
    let capture: IAudioCaptureClient = client.GetService()?;
    client.Start()?;
    let _ = ready.send(Ok(()));

    let mut player: Option<Player> = None;
    let mut peak = 0f32;
    let mut last_report = Instant::now();
    let mut loud_since: Option<Instant> = None;
    while !stop.load(Ordering::Relaxed) {
        if WaitForSingleObject(event, 100) != WAIT_OBJECT_0 {
            continue;
        }
        loop {
            let mut data = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            if capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None).is_err() || frames == 0 {
                break;
            }
            let n = frames as usize * CHANNELS as usize;
            let silent = flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;
            let samples: &[i16] = if silent { &[] } else { std::slice::from_raw_parts(data as *const i16, n) };
            for &s in samples {
                peak = peak.max((s as f32 / i16::MAX as f32).abs());
            }
            // Retorno no fone (abre o tocador só quando pedem).
            if monitor.load(Ordering::Relaxed) {
                if player.is_none() {
                    player = Player::start(speaker.clone(), 100).ok();
                }
                if let Some(p) = &player {
                    if silent {
                        p.push(&vec![0; n]);
                    } else {
                        p.push(samples);
                    }
                }
            } else {
                player = None;
            }
            capture.ReleaseBuffer(frames)?;
        }
        if last_report.elapsed() >= Duration::from_millis(50) {
            // Microfonia: com o retorno ligado, volume quase no máximo por mais de 1,5 s.
            if monitor.load(Ordering::Relaxed) && peak > 0.92 {
                let since = *loud_since.get_or_insert_with(Instant::now);
                if since.elapsed() > Duration::from_millis(1500) {
                    monitor.store(false, Ordering::Relaxed);
                    player = None;
                    loud_since = None;
                    on_event(Event::Feedback);
                }
            } else {
                loud_since = None;
            }
            on_event(Event::Level(peak.min(1.0)));
            peak = 0.0;
            last_report = Instant::now();
        }
    }
    let _ = client.Stop();
    let _ = CloseHandle(event);
    Ok(())
}
