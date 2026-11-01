//! Sons do app, gerados aqui mesmo (sem arquivos) e tocados pelo Windows (PlaySound), sem abrir
//! o áudio do WebRTC:
//!   - toque de ligação (em loop, enquanto alguém me liga pela DM);
//!   - "plim" de mensagem nova;
//!   - sons da chamada (mutar, ensurdecer, entrar/sair, alguém entrou/saiu), como no Discord.
//! O PlaySound toca um som por vez: enquanto o telefone toca, o "plim" fica quieto.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use windows::core::PCWSTR;
use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_FLAGS, SND_LOOP, SND_MEMORY, SND_NODEFAULT};

const RATE: u32 = 22_050;
static RINGING: AtomicBool = AtomicBool::new(false);

/// Nota com volume subindo/descendo suave (sem estalos). `decay` > 0 = vai sumindo (tipo sino).
fn tone(freq: f32, secs: f32, volume: f32, decay: f32) -> Vec<i16> {
    let n = (RATE as f32 * secs) as usize;
    (0..n)
        .map(|i| {
            let t = i as f32 / RATE as f32;
            let env = (t / 0.01).min(1.0).min((secs - t) / 0.01).max(0.0) * (-decay * t).exp();
            let s = (t * freq * std::f32::consts::TAU).sin() * 0.8 + (t * freq * 2.0 * std::f32::consts::TAU).sin() * 0.2;
            (s * env * volume * i16::MAX as f32) as i16
        })
        .collect()
}

fn silence(secs: f32) -> Vec<i16> {
    vec![0; (RATE as f32 * secs) as usize]
}

/// Monta um .wav (PCM 16 bits, mono) na memória.
fn wav(samples: &[i16]) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&RATE.to_le_bytes());
    out.extend_from_slice(&(RATE * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// Os sons ficam em `static`: o Windows lê dessa memória enquanto toca.
fn play(data: &'static [u8], looping: bool) {
    let flags = SND_MEMORY | SND_ASYNC | SND_NODEFAULT | if looping { SND_LOOP } else { SND_FLAGS(0) };
    unsafe {
        let _ = PlaySoundW(PCWSTR(data.as_ptr() as *const u16), None, flags);
    }
}

/// Telefone tocando: dois bipes curtos e uma pausa, em loop.
pub fn ring_start() {
    static WAV: OnceLock<Vec<u8>> = OnceLock::new();
    let data = WAV.get_or_init(|| wav(&[tone(880.0, 0.16, 0.35, 0.0), silence(0.08), tone(1175.0, 0.16, 0.35, 0.0), silence(1.4)].concat()));
    RINGING.store(true, Ordering::SeqCst);
    play(data, true);
}

pub fn ring_stop() {
    RINGING.store(false, Ordering::SeqCst);
    unsafe {
        let _ = PlaySoundW(PCWSTR::null(), None, SND_FLAGS(0));
    }
}

/// Sons da chamada. Cada um é montado uma vez (na primeira vez que toca) e fica na memória.
#[derive(Clone, Copy)]
pub enum Cue {
    Mute,
    Unmute,
    Deafen,
    Undeafen,
    Join,
    Leave,
    PeerJoin,
    PeerLeave,
}

pub fn cue(c: Cue) {
    if RINGING.load(Ordering::SeqCst) || !crate::settings::get().call_sounds {
        return;
    }
    // Notas curtas com "cauda" de sino; volume baixo para não assustar no fone.
    let n = |f: f32, secs: f32| tone(f, secs, 0.22, 14.0);
    static WAVS: OnceLock<Vec<Vec<u8>>> = OnceLock::new();
    let wavs = WAVS.get_or_init(|| {
        vec![
            wav(&[n(740.0, 0.07), n(494.0, 0.14)].concat()),                  // mutar: desce
            wav(&[n(494.0, 0.07), n(740.0, 0.14)].concat()),                  // desmutar: sobe
            wav(&[n(587.0, 0.06), n(440.0, 0.06), n(294.0, 0.16)].concat()),  // ensurdecer
            wav(&[n(294.0, 0.06), n(440.0, 0.06), n(587.0, 0.16)].concat()),  // voltar a ouvir
            wav(&[n(523.0, 0.08), n(659.0, 0.08), n(784.0, 0.2)].concat()),   // entrei
            wav(&[n(784.0, 0.08), n(659.0, 0.08), n(523.0, 0.2)].concat()),   // saí
            wav(&[tone(659.0, 0.08, 0.16, 12.0), tone(880.0, 0.18, 0.16, 12.0)].concat()), // alguém entrou
            wav(&[tone(880.0, 0.08, 0.16, 12.0), tone(659.0, 0.18, 0.16, 12.0)].concat()), // alguém saiu
        ]
    });
    play(&wavs[c as usize], false);
}

/// "Plim" de mensagem nova: duas notas curtas subindo, tipo sino.
pub fn notify() {
    if RINGING.load(Ordering::SeqCst) {
        return;
    }
    static WAV: OnceLock<Vec<u8>> = OnceLock::new();
    let data = WAV.get_or_init(|| wav(&[tone(988.0, 0.09, 0.3, 8.0), tone(1319.0, 0.22, 0.3, 9.0)].concat()));
    play(data, false);
}
