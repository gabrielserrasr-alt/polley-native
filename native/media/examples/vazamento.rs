//! Mede se ligar/desligar câmera e transmissão de tela várias vezes deixa memória presa.
//!   cargo run --release -p polley-media --example vazamento -- camera
//!   cargo run --release -p polley-media --example vazamento -- tela
use std::time::Duration;
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::Threading::GetCurrentProcess;

fn privada_mb() -> f64 {
    let mut c = PROCESS_MEMORY_COUNTERS_EX::default();
    c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
    unsafe {
        let _ = GetProcessMemoryInfo(GetCurrentProcess(), &mut c as *mut _ as *mut PROCESS_MEMORY_COUNTERS, c.cb);
    }
    c.PrivateUsage as f64 / 1_048_576.0
}

fn main() {
    let modo = std::env::args().nth(1).unwrap_or_else(|| "camera".into());
    let voltas: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(12);
    println!("início: {:.1} MB", privada_mb());
    for i in 1..=voltas {
        match modo.as_str() {
            "tela" => {
                let s = polley_media::stream::Stream::start(polley_media::stream::StreamConfig::default()).expect("tela");
                let tmp = std::env::temp_dir().join("polley-vazamento.h264");
                let mut s = s;
                s.attach(polley_media::stream::Sink::File(tmp));
                std::thread::sleep(Duration::from_millis(1500));
                s.stop().expect("parar");
            }
            _ => {
                let cam = polley_media::camera::Camera::start(0, |_f| {}).expect("câmera");
                std::thread::sleep(Duration::from_millis(1500));
                drop(cam);
            }
        }
        std::thread::sleep(Duration::from_millis(500));
        println!("volta {i:2}: {:.1} MB", privada_mb());
    }
}
