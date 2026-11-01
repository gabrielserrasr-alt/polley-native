//! Áudio do Polley nativo.
//!
//! - Microfone + saída de áudio: `PlatformAudio` do LiveKit (módulo de áudio do WebRTC, com
//!   cancelamento de eco, supressão de ruído e ganho automático). Ele também toca a voz dos
//!   outros participantes no fone/alto-falante automaticamente.
//! - Som do PC para a live: captura "process loopback" do Windows 10 2004+/11, que grava
//!   TODO o áudio do sistema EXCETO o do próprio Polley — assim a voz dos amigos tocando na
//!   call não volta pra eles pela transmissão (eco).

use anyhow::{bail, Context, Result};
use livekit::webrtc::audio_frame::AudioFrame;
use livekit::webrtc::audio_source::native::NativeAudioSource;
use livekit::{AudioProcessingOptions, PlatformAudio};
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use windows::core::{implement, Interface, Ref};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0};
use windows::Win32::System::Com::{IAgileObject, IAgileObject_Impl, BLOB};
use windows::Win32::System::Threading::{CreateEventW, GetCurrentProcessId, SetEvent, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u32 = 2;
/// WebRTC trabalha com quadros de 10 ms.
const FRAME: usize = (SAMPLE_RATE / 100) as usize;

// ---------------------------------------------------------------- microfone

pub struct Mic {
    pub audio: PlatformAudio,
}

/// Liga o módulo de áudio do WebRTC. Precisa ser chamado ANTES de conectar na sala,
/// senão o áudio dos outros não é tocado.
pub fn platform_audio(mic: Option<usize>, speaker: Option<usize>) -> Result<Mic> {
    let audio = PlatformAudio::new().map_err(|e| anyhow::anyhow!("não consegui abrir o áudio: {e}"))?;
    let recs: Vec<_> = audio.recording_devices().collect();
    let plays: Vec<_> = audio.playout_devices().collect();

    println!("Microfones:");
    for d in &recs {
        println!("  [{}] {}", d.index, d.name);
    }
    println!("Saídas de áudio:");
    for d in &plays {
        println!("  [{}] {}", d.index, d.name);
    }
    if let Some(i) = mic {
        let d = recs.iter().find(|d| d.index as usize == i).context("--mic inválido")?;
        audio.set_recording_device(&d.id).map_err(|e| anyhow::anyhow!("{e}"))?;
        println!("Usando microfone: {}", d.name);
    }
    if let Some(i) = speaker {
        let d = plays.iter().find(|d| d.index as usize == i).context("--speaker inválido")?;
        audio.set_playout_device(&d.id).map_err(|e| anyhow::anyhow!("{e}"))?;
        println!("Usando saída: {}", d.name);
    }
    audio
        .configure_audio_processing(AudioProcessingOptions {
            echo_cancellation: true,
            noise_suppression: true,
            auto_gain_control: true,
            prefer_hardware_processing: false,
        })
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(Mic { audio })
}

// ---------------------------------------------------------------- som do PC

/// Handler exigido pela ativação assíncrona do WASAPI; só avisa que terminou.
#[implement(IActivateAudioInterfaceCompletionHandler, IAgileObject)]
struct Activated(HANDLE);

impl IAgileObject_Impl for Activated_Impl {}

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(&self, _op: Ref<IActivateAudioInterfaceAsyncOperation>) -> windows::core::Result<()> {
        unsafe { SetEvent(self.0) }
    }
}

/// Abre um IAudioClient que captura o áudio do sistema inteiro menos deste processo
/// (`only = None`) ou só o de um programa e dos processos filhos dele (`only = Some(pid)`).
unsafe fn open_process_loopback(only: Option<u32>) -> Result<IAudioClient> {
    let (pid, mode) = match only {
        Some(pid) => (pid, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE),
        None => (GetCurrentProcessId(), PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE),
    };
    let params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS { TargetProcessId: pid, ProcessLoopbackMode: mode },
        },
    };
    // ManuallyDrop: o PROPVARIANT aponta pra memória da pilha; não pode ser "liberado".
    let prop = ManuallyDrop::new(PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_BLOB,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 {
                    blob: BLOB {
                        cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                        pBlobData: &params as *const _ as *mut u8,
                    },
                },
            }),
        },
    });

    let done = CreateEventW(None, false, false, None)?;
    let handler: IActivateAudioInterfaceCompletionHandler = Activated(done).into();
    let op = ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, &IAudioClient::IID, Some(&*prop), &handler)?;
    let waited = WaitForSingleObject(done, 5000);
    let _ = CloseHandle(done);
    if waited != WAIT_OBJECT_0 {
        bail!("o Windows não respondeu à captura de áudio do sistema");
    }
    let mut hr = windows::core::HRESULT(0);
    let mut unknown = None;
    op.GetActivateResult(&mut hr, &mut unknown)?;
    hr.ok().context("captura por processo recusada (precisa do Windows 10 2004 ou mais novo)")?;
    Ok(unknown.context("sem interface de áudio")?.cast()?)
}

/// Captura o som do PC numa thread e entrega quadros de 10 ms (48 kHz, estéreo, i16).
pub fn start_system_audio(stop: Arc<AtomicBool>) -> Result<mpsc::Receiver<Vec<i16>>> {
    start_loopback(stop, None)
}

/// Igual a [`start_system_audio`], mas só com o som de um programa (o da janela transmitida).
pub fn start_app_audio(stop: Arc<AtomicBool>, pid: u32) -> Result<mpsc::Receiver<Vec<i16>>> {
    start_loopback(stop, Some(pid))
}

fn start_loopback(stop: Arc<AtomicBool>, only: Option<u32>) -> Result<mpsc::Receiver<Vec<i16>>> {
    let (tx, rx) = mpsc::sync_channel::<Vec<i16>>(50);
    let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();

    std::thread::spawn(move || {
        let run = || -> Result<()> {
            unsafe {
                windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED).ok()?;
                let client = open_process_loopback(only)?;
                // Na captura por processo não existe "formato padrão": dizemos qual queremos.
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
                    AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
                    200_000, // buffer de 20 ms
                    0,
                    &format,
                    None,
                )?;
                let event = CreateEventW(None, false, false, None)?;
                client.SetEventHandle(event)?;
                let capture: IAudioCaptureClient = client.GetService()?;
                client.Start()?;
                let _ = ready_tx.send(Ok(()));

                let chunk = FRAME * CHANNELS as usize;
                let mut pending: Vec<i16> = Vec::with_capacity(chunk * 4);
                while !stop.load(Ordering::Relaxed) {
                    if WaitForSingleObject(event, 20) != WAIT_OBJECT_0 {
                        // Nada tocando no PC: o Windows para de mandar pacotes.
                        // Mandamos silêncio pra manter o ritmo de 10 ms.
                        for _ in 0..2 {
                            if tx.try_send(vec![0; chunk]).is_err() {
                                break;
                            }
                        }
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
                        if flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 {
                            pending.extend(std::iter::repeat(0).take(n));
                        } else {
                            pending.extend_from_slice(std::slice::from_raw_parts(data as *const i16, n));
                        }
                        capture.ReleaseBuffer(frames)?;
                        while pending.len() >= chunk {
                            let rest = pending.split_off(chunk);
                            // Se ninguém estiver consumindo, descarta (nunca acumula RAM).
                            let _ = tx.try_send(std::mem::replace(&mut pending, rest));
                        }
                    }
                }
                let _ = client.Stop();
                let _ = CloseHandle(event);
            }
            Ok(())
        };
        if let Err(e) = run() {
            let _ = ready_tx.send(Err(e));
        }
    });

    ready_rx.recv().context("thread de áudio morreu")??;
    Ok(rx)
}

/// Empurra os quadros do som do PC para a trilha "áudio da tela" do LiveKit
/// (e para a gravação, se houver uma ligada).
pub async fn pump_system_audio(
    rx: mpsc::Receiver<Vec<i16>>,
    source: NativeAudioSource,
    recording: Option<Arc<crate::record::Recording>>,
) {
    let (atx, mut arx) = tokio::sync::mpsc::channel::<Vec<i16>>(50);
    // Ponte da thread do WASAPI (bloqueante) para o mundo async.
    std::thread::spawn(move || {
        // A gravação usa o Media Foundation (COM).
        let _ = unsafe {
            windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED)
        };
        while let Ok(frame) = rx.recv() {
            if let Some(r) = &recording {
                r.audio(&frame);
            }
            if atx.blocking_send(frame).is_err() {
                break;
            }
        }
    });
    while let Some(data) = arx.recv().await {
        let frame = AudioFrame {
            data: data.into(),
            sample_rate: SAMPLE_RATE,
            num_channels: CHANNELS,
            samples_per_channel: FRAME as u32,
        };
        let _ = source.capture_frame(&frame).await;
    }
}
