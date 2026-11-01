//! Protótipo do Polley nativo: transmissão de tela até 4K gastando o mínimo de RAM.
//!
//! Uso:
//!   stream-proto record [--res 2160] [--fps 60] [--seconds 20] [--out teste.h264 | teste.mp4]
//!   stream-proto live --email voce@exemplo.com [--res 2160] [--fps 60]
//!
//! Pipeline (a imagem nunca sai da placa de vídeo até estar comprimida):
//!   tela → WGC (textura GPU) → Video Processor (NV12 + escala, GPU) → encoder H.264 (GPU) → bytes

use polley_media::stream::{Sink, Stream, StreamConfig};
use polley_media::{audio, mem};
mod live;

use anyhow::{bail, Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};


// Em notebooks com duas placas (NVIDIA Optimus / AMD), estes símbolos exportados pedem ao driver
// para rodar o programa na placa dedicada — sem isso o encoder da NVIDIA recusa ativar.
#[no_mangle]
pub static NvOptimusEnablement: u32 = 1;
#[no_mangle]
pub static AmdPowerXpressRequestHighPerformance: u32 = 1;

struct Args {
    mode: String,
    res: u32,
    fps: u32,
    seconds: u64,
    out: String,
    email: Option<String>,
    gpu: Option<String>,
    mic: Option<usize>,
    speaker: Option<usize>,
    no_mic: bool,
    no_system_audio: bool,
    /// Força a resolução pedida mesmo que a tela seja menor (para medir o pior caso).
    force: bool,
}

fn parse_args() -> Result<Args> {
    let mut it = std::env::args().skip(1);
    let mode = it.next().unwrap_or_else(|| "record".into());
    let mut a = Args { mode, res: 2160, fps: 60, seconds: 20, out: "teste.h264".into(), email: None, gpu: None, force: false,
        mic: None, speaker: None, no_mic: false, no_system_audio: false };
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--force" => { a.force = true; continue; }
            "--sem-mic" => { a.no_mic = true; continue; }
            "--sem-som-pc" => { a.no_system_audio = true; continue; }
            _ => {}
        }
        let val = it.next().with_context(|| format!("{flag} precisa de um valor"))?;
        match flag.as_str() {
            "--res" => a.res = val.trim_end_matches('p').parse()?,
            "--fps" => a.fps = val.parse()?,
            "--seconds" => a.seconds = val.parse()?,
            "--out" => a.out = val,
            "--email" => a.email = Some(val),
            "--gpu" => a.gpu = Some(val.to_lowercase()),
            "--mic" => a.mic = Some(val.parse()?),
            "--speaker" => a.speaker = Some(val.parse()?),
            _ => bail!("opção desconhecida: {flag}"),
        }
    }
    if !matches!(a.fps, 30 | 60) {
        bail!("--fps precisa ser 30 ou 60");
    }
    Ok(a)
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    // Terminal do Windows em UTF-8 (senão "não" vira "nÃ£o").
    unsafe {
        let _ = windows::Win32::System::Console::SetConsoleOutputCP(65001);
    }
    // Duas libs trazem motores de TLS diferentes (ring e aws-lc); escolhemos um explicitamente.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let args = parse_args()?;
    let base = mem::current();
    println!("RAM ao iniciar: {:.1} MB", base.task_manager_mb);

    // Live: login e escolha do canal antes de começar a capturar.
    let live = if args.mode == "live" {
        let cfg = live::load_config()?;
        let email = args.email.clone().context("use --email seu@email.com")?;
        let (token, channel) = live::get_voice_token(&cfg, &email).await?;
        println!("Entrando em {channel}...");
        Some((cfg, token))
    } else if args.mode == "record" {
        None
    } else {
        bail!("modo precisa ser 'record' ou 'live'");
    };

    let stop = Arc::new(AtomicBool::new(false));

    // Captura + encoder numa thread própria (tenta cada placa de vídeo até uma servir).
    let cfg = StreamConfig { height: args.res, fps: args.fps, force: args.force, gpu: args.gpu.clone(), verbose: true, target: None };
    let mut stream = tokio::task::spawn_blocking(move || Stream::start(cfg)).await??;
    let (w, h, bitrate) = (stream.info.width, stream.info.height, stream.info.bitrate);
    println!("RAM depois de ligar GPU+encoder: {:.1} MB", mem::current().task_manager_mb);
    println!("Encoder: {}\nTransmitindo {w}x{h} @ {} fps, {:.1} Mbps", stream.info.encoder, args.fps, bitrate as f64 / 1e6);

    // Áudio: o módulo do WebRTC precisa existir ANTES de conectar na sala.
    let mic = if live.is_some() {
        Some(audio::platform_audio(args.mic, args.speaker)?)
    } else {
        None
    };
    let system_audio = if live.is_some() && !args.no_system_audio {
        match audio::start_system_audio(stop.clone()) {
            Ok(rx) => Some(rx),
            Err(e) => {
                println!("Som do PC desligado: {e:#}");
                None
            }
        }
    } else {
        None
    };

    let _room = if let Some((cfg, token)) = &live {
        let mic_for_publish = if args.no_mic { None } else { mic.as_ref() };
        let published = live::publish(cfg, token, w, h, args.fps, bitrate, mic_for_publish, system_audio).await?;
        stream.attach(Sink::LiveKit(published.video));
        println!(
            "AO VIVO! Microfone: {} | Som do PC: {}",
            if published.mic.is_some() { "ligado" } else { "desligado" },
            if args.no_system_audio { "desligado" } else { "ligado" }
        );
        println!("Digite m + Enter para mutar/desmutar. Ctrl+C para parar.");

        // Mutar pelo terminal.
        if let Some(track) = published.mic.clone() {
            // mute()/unmute() do LiveKit precisam do runtime tokio: a thread "entra" nele.
            let runtime = tokio::runtime::Handle::current();
            std::thread::spawn(move || {
                let _guard = runtime.enter();
                let mut muted = false;
                for line in std::io::stdin().lines().map_while(Result::ok) {
                    if line.trim().eq_ignore_ascii_case("m") {
                        muted = !muted;
                        if muted { track.mute() } else { track.unmute() }
                        println!("{}", if muted { "🔇 microfone mutado" } else { "🎤 microfone ligado" });
                    }
                }
            });
        }
        Some(published.room)
    } else if args.out.ends_with(".mp4") {
        // .mp4 com o som do PC: o mesmo caminho do botão "Gravar" do app.
        stream.recording.start(std::path::absolute(&args.out)?, true)?;
        let rx = audio::start_system_audio(stop.clone())?;
        let rec = stream.recording.clone();
        std::thread::spawn(move || {
            let _ = unsafe {
                windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED)
            };
            while let Ok(frame) = rx.recv() {
                rec.audio(&frame);
            }
        });
        stream.attach(Sink::File("NUL".into())); // só o .mp4 interessa
        println!("Gravando {} s em {} (com o som do PC) ...", args.seconds, args.out);
        None
    } else {
        stream.attach(Sink::File(args.out.clone().into()));
        println!("Gravando {} s em {} ...", args.seconds, args.out);
        None
    };

    // Relatório a cada 2 s até acabar o tempo (record) ou Ctrl+C.
    let deadline = if live.is_some() { None } else { Some(Instant::now() + Duration::from_secs(args.seconds)) };
    let mut peak = 0f64;
    let (mut last_bytes, mut last_frames) = (0u64, 0u64);
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
        }
        if !stream.is_running() {
            break;
        }
        let m = mem::current();
        peak = peak.max(m.task_manager_mb);
        let (b, f) = (stream.bytes.load(Ordering::Relaxed), stream.frames.load(Ordering::Relaxed));
        println!(
            "RAM {:>6.1} MB (pico {:>6.1}, confirmada {:>6.1}) | {:>3.0} fps | {:>5.1} Mbps",
            m.task_manager_mb,
            peak,
            m.commit_mb,
            (f - last_frames) as f64 / 2.0,
            (b - last_bytes) as f64 * 8.0 / 2.0 / 1e6
        );
        (last_bytes, last_frames) = (b, f);
        if deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }
    }

    stop.store(true, Ordering::Relaxed);
    if let Some(r) = stream.recording.stop()? {
        println!("Arquivo: {} ({:.1} s)", r.path.display(), r.duration.as_secs_f64());
    }
    if let Some(e) = stream.recording.take_error() {
        println!("Erro na gravação: {e}");
    }
    let (frames, bytes) = (stream.frames.clone(), stream.bytes.clone());
    tokio::task::spawn_blocking(move || stream.stop()).await??;
    println!(
        "\nFim. {} quadros, {:.1} MB de vídeo. Pico de RAM: {:.1} MB (meta: 150 MB)",
        frames.load(Ordering::Relaxed),
        bytes.load(Ordering::Relaxed) as f64 / 1e6,
        peak
    );
    Ok(())
}
