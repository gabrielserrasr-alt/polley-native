//! Mostra a live assistida numa janelinha própria, encaixada em cima da área da live, sem
//! passar pela interface.
//!
//! Antes, a interface (que desenha tudo no processador, pixel por pixel) pintava ~1 milhão de
//! pixels 60x por segundo: quase um núcleo inteiro só para isso. Agora o quadro é reduzido e
//! convertido de uma vez (libyuv, com SIMD) e o Windows copia para a tela (GDI).
//! Medido (examples/live_ram, 1080p60): ~30% de um núcleo e +6 MB de RAM, que voltam ao parar.
//! O modo `POLLEY_VIDEO=gpu` usa a placa de vídeo: um pouco menos de processador, mas o driver
//! da placa custa ~50 MB de RAM e ~28 MB ficam presos depois — por isso não é o padrão.

use crate::gpu;
use anyhow::{Context, Result};
use livekit::webrtc::native::yuv_helper;
use livekit::webrtc::prelude::VideoBuffer;
use livekit::webrtc::video_frame::native::VideoFrameBufferExt;
use std::cell::Cell;
use std::mem::ManuallyDrop;
use livekit::webrtc::video_frame::VideoFormatType;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use windows::core::{w, Interface};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, TRUE, WPARAM};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::{
    FillRect, GetDC, GetStockObject, MonitorFromWindow, ReleaseDC, SetDIBitsToDevice, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    BLACK_BRUSH, DIB_RGB_COLORS, HBRUSH, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

/// Um quadro em NV12 contínuo (Y e depois UV intercalado), largura e altura pares.
struct Nv12 {
    w: u32,
    h: u32,
    data: Vec<u8>,
}

/// Um quadro já no tamanho da janelinha, em BGRA (o formato de imagem do Windows).
struct Bgra {
    w: u32,
    h: u32,
    data: Vec<u8>,
}

enum Frame {
    Nv12(Nv12),
    Bgra(Bgra),
}

/// Quem desenha a live.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// O próprio Windows (GDI): o processador reduz e converte o quadro (rápido, com SIMD) e o
    /// Windows copia para a tela. Não carrega driver de placa de vídeo = pouca RAM.
    Gdi,
    /// A placa de vídeo converte e redimensiona. Menos processador, mas o driver da placa
    /// custa ~45 MB de RAM enquanto assiste (e parte fica presa depois).
    Gpu,
}

fn mode() -> Mode {
    if std::env::var("POLLEY_VIDEO").is_ok_and(|v| v == "gpu") {
        Mode::Gpu
    } else {
        Mode::Gdi
    }
}

enum Cmd {
    Frame(Frame),
    /// Tamanho da janelinha em pixels.
    Size(u32, u32),
    /// Pinta de preto e esquece o último quadro (troca de live).
    Clear,
    Stop,
}

/// Entrada de quadros (pode ser usada de qualquer thread).
#[derive(Clone)]
pub struct VideoSink {
    tx: mpsc::Sender<Cmd>,
    /// A placa ainda está desenhando o quadro anterior? Então o novo é pulado (sem fila = sem atraso).
    busy: Arc<AtomicBool>,
    mode: Mode,
    /// Tamanho da janelinha em pixels (no modo GDI o quadro já chega desse tamanho).
    target: Arc<(AtomicU32, AtomicU32)>,
}

impl VideoSink {
    /// Manda o quadro para a placa. Devolve `false` se ele foi pulado.
    pub fn push(&self, buffer: &dyn VideoBuffer) -> bool {
        if self.busy.load(Ordering::SeqCst) {
            return false;
        }
        let frame = match self.mode {
            Mode::Gpu => to_nv12(buffer).map(Frame::Nv12),
            Mode::Gdi => {
                let target = (self.target.0.load(Ordering::Relaxed), self.target.1.load(Ordering::Relaxed));
                to_bgra(buffer, target).map(Frame::Bgra)
            }
        };
        let Some(frame) = frame else { return false };
        self.busy.store(true, Ordering::SeqCst);
        if self.tx.send(Cmd::Frame(frame)).is_err() {
            self.busy.store(false, Ordering::SeqCst);
            return false;
        }
        true
    }
}

/// Reduz o quadro para caber em `target` (sem distorcer) e converte para BGRA.
fn to_bgra(buffer: &dyn VideoBuffer, target: (u32, u32)) -> Option<Bgra> {
    let (fw, fh) = (buffer.width(), buffer.height());
    if target.0 < 2 || target.1 < 2 || fw < 2 || fh < 2 {
        return None;
    }
    let scale = f64::min(target.0 as f64 / fw as f64, target.1 as f64 / fh as f64);
    let w = (((fw as f64 * scale) as u32) & !1).max(2);
    let h = (((fh as f64 * scale) as u32) & !1).max(2);
    let mut i420 = if let Some(b) = buffer.as_i420() {
        b.to_i420()
    } else if let Some(b) = buffer.as_native() {
        b.to_i420()
    } else if let Some(b) = buffer.as_nv12() {
        b.to_i420()
    } else {
        return None;
    };
    let scaled = if (w, h) != (fw, fh) { i420.scale(w as i32, h as i32) } else { i420 };
    let mut data = vec![0u8; (w * h * 4) as usize];
    // "ARGB" no libyuv = bytes B,G,R,A na memória = o que o Windows espera.
    scaled.to_argb(VideoFormatType::ARGB, &mut data, w * 4, w as i32, h as i32);
    Some(Bgra { w, h, data })
}

fn to_nv12(buffer: &dyn VideoBuffer) -> Option<Nv12> {
    let (w, h) = (buffer.width() & !1, buffer.height() & !1);
    if w < 2 || h < 2 {
        return None;
    }
    let mut data = vec![0u8; (w * h * 3 / 2) as usize];
    let (dst_y, dst_uv) = data.split_at_mut((w * h) as usize);
    if let Some(b) = buffer.as_nv12() {
        // Já vem em NV12: só tira as sobras do fim das linhas.
        let (sy, suv) = b.strides();
        let (y, uv) = b.data();
        let (wu, sy, suv) = (w as usize, sy as usize, suv as usize);
        for row in 0..h as usize {
            dst_y[row * wu..(row + 1) * wu].copy_from_slice(&y[row * sy..row * sy + wu]);
        }
        for row in 0..h as usize / 2 {
            dst_uv[row * wu..(row + 1) * wu].copy_from_slice(&uv[row * suv..row * suv + wu]);
        }
    } else {
        let owned;
        let i420 = if let Some(b) = buffer.as_i420() {
            b
        } else if let Some(b) = buffer.as_native() {
            owned = b.to_i420();
            &owned
        } else {
            return None;
        };
        let (sy, su, sv) = i420.strides();
        let (y, u, v) = i420.data();
        yuv_helper::i420_to_nv12(y, sy, u, su, v, sv, dst_y, w, dst_uv, w, w as i32, h as i32);
    }
    Some(Nv12 { w, h, data })
}

/// A janelinha da live. Criar e mexer só na thread da interface (a dona da janela principal).
pub struct VideoView {
    hwnd: HWND,
    sink: VideoSink,
    thread: Option<JoinHandle<()>>,
    /// Última posição aplicada (x, y, largura, altura, visível): só mexe na janela se mudar.
    placed: Cell<(i32, i32, i32, i32, bool)>,
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        // O mouse "atravessa" a live e chega na janela principal (clique duplo, cursor, botões).
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_ERASEBKGND => LRESULT(1),
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

impl VideoView {
    /// Cria a janelinha (escondida) dentro de `parent` e liga a placa de vídeo.
    pub fn new(parent: HWND) -> Result<Self> {
        let hwnd = unsafe {
            let module = GetModuleHandleW(None)?;
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: module.into(),
                lpszClassName: w!("PolleyVideo"),
                hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
                ..Default::default()
            };
            // Na segunda vez a classe já existe: tudo bem.
            RegisterClassW(&wc);
            // A janela principal não pinta por cima da filha.
            let style = GetWindowLongPtrW(parent, GWL_STYLE);
            SetWindowLongPtrW(parent, GWL_STYLE, style | WS_CLIPCHILDREN.0 as isize);
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("PolleyVideo"),
                w!(""),
                WS_CHILD | WS_CLIPSIBLINGS,
                0,
                0,
                16,
                16,
                Some(parent),
                None,
                Some(module.into()),
                None,
            )?
        };
        let (tx, rx) = mpsc::channel();
        let busy = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::channel();
        let raw = hwnd.0 as isize;
        let mode = mode();
        let target = Arc::new((AtomicU32::new(0), AtomicU32::new(0)));
        let thread = {
            let busy = busy.clone();
            std::thread::Builder::new().name("polley-video-view".into()).spawn(move || {
                let hwnd = HWND(raw as *mut _);
                let backend = match mode {
                    Mode::Gdi => Ok(Backend::Gdi(Gdi { hwnd, out: (16, 16), last: None, drawn: (0, 0) })),
                    Mode::Gpu => Renderer::new(hwnd).map(Backend::Gpu),
                };
                match backend {
                    Ok(mut r) => {
                        let _ = ready_tx.send(Ok(()));
                        r.run(rx, &busy);
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                    }
                }
            })?
        };
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self { hwnd, sink: VideoSink { tx, busy, mode, target }, thread: Some(thread), placed: Cell::new((0, 0, 0, 0, false)) }),
            Ok(Err(e)) => {
                let _ = thread.join();
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                Err(e)
            }
            Err(_) => {
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                anyhow::bail!("a placa de vídeo parou sem explicar")
            }
        }
    }

    pub fn sink(&self) -> VideoSink {
        self.sink.clone()
    }

    /// Encaixa a janelinha na área da live (pixels da janela principal) ou esconde.
    pub fn place(&self, x: i32, y: i32, w: i32, h: i32, visible: bool) {
        let (w, h) = (w.max(2), h.max(2));
        let now = (x, y, w, h, visible);
        let before = self.placed.replace(now);
        if before == now {
            return;
        }
        if (before.2, before.3) != (w, h) {
            self.sink.target.0.store(w as u32, Ordering::Relaxed);
            self.sink.target.1.store(h as u32, Ordering::Relaxed);
            let _ = self.sink.tx.send(Cmd::Size(w as u32, h as u32));
        }
        if before.4 && !visible {
            let _ = self.sink.tx.send(Cmd::Clear);
        }
        let show = if visible { SWP_SHOWWINDOW } else { SWP_HIDEWINDOW };
        unsafe {
            let _ = SetWindowPos(self.hwnd, None, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE | show);
        }
    }
}

impl Drop for VideoView {
    fn drop(&mut self) {
        let _ = self.sink.tx.send(Cmd::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// Placas de vídeo com a que está ligada ao monitor da janela primeiro: é ela que mostra a
/// imagem, então não precisa copiar de uma placa para outra, nem carregar o driver da outra
/// (o driver da NVIDIA sozinho custa dezenas de MB de RAM num notebook com as duas).
/// `POLLEY_VIDEO_GPU=nvidia|intel|amd` força uma (para testes).
fn display_first(hwnd: HWND) -> Result<Vec<gpu::Adapter>> {
    let mut list = gpu::adapters()?;
    let forced = std::env::var("POLLEY_VIDEO_GPU").ok().map(|s| s.to_lowercase());
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    list.sort_by_key(|a| {
        if let Some(f) = &forced {
            return u8::from(!a.name.to_lowercase().contains(f.as_str()));
        }
        let mut i = 0;
        while let Ok(output) = unsafe { a.adapter.EnumOutputs(i) } {
            i += 1;
            if unsafe { output.GetDesc() }.is_ok_and(|d| d.Monitor == monitor) {
                return 0;
            }
        }
        1
    });
    Ok(list)
}

enum Backend {
    Gdi(Gdi),
    Gpu(Renderer),
}

impl Backend {
    fn run(&mut self, rx: mpsc::Receiver<Cmd>, busy: &AtomicBool) {
        while let Ok(cmd) = rx.recv() {
            match (&mut *self, cmd) {
                (_, Cmd::Stop) => break,
                (Backend::Gdi(g), Cmd::Frame(Frame::Bgra(f))) => g.frame(f),
                (Backend::Gpu(r), Cmd::Frame(Frame::Nv12(f))) => r.frame(&f),
                (_, Cmd::Frame(_)) => {}
                (Backend::Gdi(g), cmd) => g.command(cmd),
                (Backend::Gpu(r), cmd) => r.command(cmd),
            }
            busy.store(false, Ordering::SeqCst);
        }
    }
}

/// Desenho pelo Windows: copia o quadro (já do tamanho certo) para a janelinha.
struct Gdi {
    hwnd: HWND,
    out: (u32, u32),
    /// Último quadro (para redesenhar ao mudar o tamanho).
    last: Option<Bgra>,
    /// Tamanho do último quadro desenhado (mudou = repinta as faixas pretas).
    drawn: (u32, u32),
}

impl Gdi {
    fn frame(&mut self, f: Bgra) {
        if (f.w, f.h) != self.drawn {
            self.fill_black();
            self.drawn = (f.w, f.h);
        }
        self.blit(&f);
        self.last = Some(f);
    }

    fn command(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Size(w, h) => {
                self.out = (w, h);
                self.fill_black();
                if let Some(f) = self.last.take() {
                    // O próximo quadro já vem no tamanho novo; até lá, o antigo centralizado.
                    self.blit(&f);
                    self.last = Some(f);
                }
            }
            Cmd::Clear => {
                self.last = None;
                self.drawn = (0, 0);
                self.fill_black();
            }
            Cmd::Frame(_) | Cmd::Stop => {}
        }
    }

    fn blit(&self, f: &Bgra) {
        let (ow, oh) = (self.out.0 as i32, self.out.1 as i32);
        let (x, y) = ((ow - f.w as i32).max(0) / 2, (oh - f.h as i32).max(0) / 2);
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: f.w as i32,
                // Negativo = linhas de cima para baixo (como o quadro vem).
                biHeight: -(f.h as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        unsafe {
            let dc = GetDC(Some(self.hwnd));
            SetDIBitsToDevice(dc, x, y, f.w, f.h, 0, 0, 0, f.h, f.data.as_ptr() as *const _, &info, DIB_RGB_COLORS);
            ReleaseDC(Some(self.hwnd), dc);
        }
    }

    fn fill_black(&self) {
        let rect = RECT { left: 0, top: 0, right: self.out.0 as i32, bottom: self.out.1 as i32 };
        unsafe {
            let dc = GetDC(Some(self.hwnd));
            FillRect(dc, &rect, HBRUSH(GetStockObject(BLACK_BRUSH).0));
            ReleaseDC(Some(self.hwnd), dc);
        }
    }
}

/// Conversor de cores/tamanho para um par (entrada, saída).
struct Processor {
    key: (u32, u32, u32, u32),
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    input_view: ID3D11VideoProcessorInputView,
}

struct Renderer {
    gpu: gpu::Gpu,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    swapchain: IDXGISwapChain1,
    out: (u32, u32),
    /// Último quadro na placa (para redesenhar ao mudar o tamanho).
    input: Option<(u32, u32, ID3D11Texture2D)>,
    processor: Option<Processor>,
}

impl Renderer {
    fn new(hwnd: HWND) -> Result<Self> {
        let mut last_err = anyhow::anyhow!("nenhuma placa de vídeo encontrada");
        let mut found = None;
        for adapter in display_first(hwnd)? {
            match gpu::Gpu::new(&adapter) {
                Ok(g) => {
                    found = Some(g);
                    break;
                }
                Err(e) => last_err = e,
            }
        }
        let gpu = found.ok_or(last_err)?;
        let video_device: ID3D11VideoDevice = gpu.device.cast()?;
        let video_context: ID3D11VideoContext = gpu.context.cast()?;
        let swapchain = unsafe {
            let factory: IDXGIFactory2 = gpu.device.cast::<IDXGIDevice>()?.GetAdapter()?.GetParent()?;
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: 16,
                Height: 16,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                ..Default::default()
            };
            let sc = factory.CreateSwapChainForHwnd(&gpu.device, hwnd, &desc, None, None).context("swapchain da live")?;
            let _ = factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER);
            sc
        };
        Ok(Self { gpu, video_device, video_context, swapchain, out: (16, 16), input: None, processor: None })
    }

    fn frame(&mut self, f: &Nv12) {
        let _ = self.upload(f).and_then(|_| self.draw());
    }

    fn command(&mut self, cmd: Cmd) {
        {
            match cmd {
                Cmd::Frame(_) | Cmd::Stop => {}
                Cmd::Size(w, h) => {
                    if (w, h) != self.out {
                        self.out = (w, h);
                        self.processor = None;
                        unsafe {
                            let _ = self.swapchain.ResizeBuffers(0, w, h, DXGI_FORMAT_UNKNOWN, DXGI_SWAP_CHAIN_FLAG(0));
                        }
                        let _ = if self.input.is_some() { self.draw() } else { self.clear() };
                    }
                }
                Cmd::Clear => {
                    self.input = None;
                    self.processor = None;
                    let _ = self.clear();
                }
            }
        }
    }

    fn upload(&mut self, f: &Nv12) -> Result<()> {
        if self.input.as_ref().is_none_or(|(w, h, _)| (*w, *h) != (f.w, f.h)) {
            self.input = Some((f.w, f.h, self.gpu.texture(f.w, f.h, DXGI_FORMAT_NV12)?));
            self.processor = None;
        }
        let (_, _, tex) = self.input.as_ref().context("sem textura")?;
        unsafe { self.gpu.context.UpdateSubresource(tex, 0, None, f.data.as_ptr() as *const _, f.w, 0) };
        Ok(())
    }

    fn ensure_processor(&mut self) -> Result<()> {
        let (iw, ih, tex) = self.input.as_ref().context("sem quadro")?;
        let (iw, ih) = (*iw, *ih);
        let (ow, oh) = self.out;
        let key = (iw, ih, ow, oh);
        if self.processor.as_ref().is_none_or(|p| p.key != key) {
            let rate = DXGI_RATIONAL { Numerator: 60, Denominator: 1 };
            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: iw,
                InputHeight: ih,
                OutputFrameRate: rate,
                OutputWidth: ow,
                OutputHeight: oh,
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            };
            let enumerator = unsafe { self.video_device.CreateVideoProcessorEnumerator(&desc)? };
            let processor = unsafe { self.video_device.CreateVideoProcessor(&enumerator, 0)? };
            let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 } },
            };
            let mut input_view = None;
            unsafe { self.video_device.CreateVideoProcessorInputView(tex, &enumerator, &in_desc, Some(&mut input_view))? };

            // Cabe na janelinha sem distorcer (faixas pretas nas sobras), centralizado.
            let scale = (ow as f64 / iw as f64).min(oh as f64 / ih as f64);
            let (dw, dh) = ((iw as f64 * scale).round() as i32, (ih as f64 * scale).round() as i32);
            let (x, y) = ((ow as i32 - dw) / 2, (oh as i32 - dh) / 2);
            let vc = &self.video_context;
            unsafe {
                // Entrada: YCbCr BT.709 16–235 (o que a nossa live manda). Saída: RGB 0–255.
                // Bits: Usage|RGB_Range|YCbCr_Matrix|xvYCC|Nominal_Range(2)
                vc.VideoProcessorSetStreamColorSpace(&processor, 0, &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: (1 << 2) | (1 << 4) });
                vc.VideoProcessorSetOutputColorSpace(&processor, &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 });
                vc.VideoProcessorSetStreamFrameFormat(&processor, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
                vc.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
                let full = RECT { left: 0, top: 0, right: ow as i32, bottom: oh as i32 };
                vc.VideoProcessorSetOutputTargetRect(&processor, true, Some(&full));
                let black = D3D11_VIDEO_COLOR { Anonymous: D3D11_VIDEO_COLOR_0 { RGBA: D3D11_VIDEO_COLOR_RGBA { R: 0.0, G: 0.0, B: 0.0, A: 1.0 } } };
                vc.VideoProcessorSetOutputBackgroundColor(&processor, false, &black);
                let src = RECT { left: 0, top: 0, right: iw as i32, bottom: ih as i32 };
                let dst = RECT { left: x, top: y, right: x + dw, bottom: y + dh };
                vc.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&src));
                vc.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&dst));
            }
            self.processor = Some(Processor { key, enumerator, processor, input_view: input_view.context("input view vazia")? });
        }
        Ok(())
    }

    fn draw(&mut self) -> Result<()> {
        let back: ID3D11Texture2D = unsafe { self.swapchain.GetBuffer(0)? };
        self.ensure_processor()?;
        let p = self.processor.as_ref().context("sem conversor")?;
        let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
        };
        let mut view = None;
        unsafe { self.video_device.CreateVideoProcessorOutputView(&back, &p.enumerator, &out_desc, Some(&mut view))? };
        let view = view.context("output view vazia")?;
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: TRUE,
            pInputSurface: ManuallyDrop::new(Some(p.input_view.clone())),
            ..Default::default()
        };
        let streams = [stream];
        let result = unsafe { self.video_context.VideoProcessorBlt(&p.processor, &view, 0, &streams) };
        let [mut stream] = streams;
        unsafe { ManuallyDrop::drop(&mut stream.pInputSurface) };
        result.context("VideoProcessorBlt falhou")?;
        // Sem esperar o monitor: se chegar outro quadro, ele já substitui este.
        unsafe { self.swapchain.Present(0, DXGI_PRESENT(0)).ok()? };
        Ok(())
    }

    fn clear(&mut self) -> Result<()> {
        let back: ID3D11Texture2D = unsafe { self.swapchain.GetBuffer(0)? };
        let mut rtv = None;
        unsafe {
            self.gpu.device.CreateRenderTargetView(&back, None, Some(&mut rtv))?;
            if let Some(rtv) = rtv {
                self.gpu.context.ClearRenderTargetView(&rtv, &[0.0, 0.0, 0.0, 1.0]);
            }
            self.swapchain.Present(0, DXGI_PRESENT(0)).ok()?;
        }
        Ok(())
    }
}
