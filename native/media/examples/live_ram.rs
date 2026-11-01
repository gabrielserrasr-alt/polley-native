//! Mede a RAM da live desenhada pela placa de vídeo (janelinha + 300 quadros 1080p).
//!   cargo run --release -p polley-media --example live_ram
//!   (POLLEY_VIDEO_GPU=nvidia ou intel força a placa)
use livekit::webrtc::video_frame::I420Buffer;
use polley_media::videoview::VideoView;
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

/// Tempo de processador gasto pelo processo até agora (ms).
fn cpu_ms() -> f64 {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let (mut a, mut b, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    unsafe {
        let _ = GetProcessTimes(GetCurrentProcess(), &mut a, &mut b, &mut k, &mut u);
    }
    let t = |f: FILETIME| ((f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64) as f64 / 10_000.0;
    t(k) + t(u)
}

fn mb() -> f64 {
    polley_media::mem::current().task_manager_mb
}

unsafe extern "system" fn proc_(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    DefWindowProcW(h, m, w, l)
}

fn pump() {
    unsafe {
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn main() -> anyhow::Result<()> {
    let parent: HWND = unsafe {
        let module = GetModuleHandleW(None)?;
        let wc = WNDCLASSW { lpfnWndProc: Some(proc_), hInstance: module.into(), lpszClassName: w!("LiveRam"), ..Default::default() };
        RegisterClassW(&wc);
        let h = CreateWindowExW(WINDOW_EX_STYLE(0), w!("LiveRam"), w!("teste"), WS_OVERLAPPEDWINDOW | WS_VISIBLE, 100, 100, 1300, 800, None, None, Some(module.into()), None)?;
        h
    };
    pump();
    println!("antes:          {:.1} MB", mb());
    for volta in 1..=3 {
        let view = VideoView::new(parent)?;
        view.place(0, 0, 1280, 720, true);
        let sink = view.sink();
        let mut frame = I420Buffer::new(1920, 1080);
        let (c0, t0) = (cpu_ms(), std::time::Instant::now());
        for i in 0..300u32 {
            let (y, u, v) = frame.data_mut();
            y.fill((i % 220) as u8 + 16);
            u.fill(128);
            v.fill(((i * 3) % 200) as u8 + 28);
            sink.push(&frame);
            pump();
            std::thread::sleep(std::time::Duration::from_millis(16));
        }
        let cpu = (cpu_ms() - c0) / t0.elapsed().as_millis() as f64 * 100.0;
        println!("volta {volta} vendo:  {:.1} MB   processador: {cpu:.0}% de um núcleo (inclui gerar o quadro de teste)", mb());
        drop(view);
        pump();
        std::thread::sleep(std::time::Duration::from_millis(500));
        println!("volta {volta} parou:  {:.1} MB", mb());
    }
    Ok(())
}
