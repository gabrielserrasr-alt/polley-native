//! Captura da tela com Windows Graphics Capture (a API que o OBS e o Discord usam no Win10/11).
//! Os quadros chegam como texturas na GPU — nenhum pixel é copiado para a RAM.
//!
//! Dá para capturar um monitor inteiro ou uma janela só. [`list_sources`] lista as opções
//! (para a janela "o que transmitir") e [`thumbnail`] tira uma miniatura de cada uma.

use anyhow::{bail, Context, Result};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use windows::core::{factory, Interface, BOOL};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Direct3D11::{ID3D11Texture2D, D3D11_BOX, D3D11_TEXTURE2D_DESC};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::gpu::Gpu;

// ---------------------------------------------------------------- o que transmitir

/// Um monitor ou uma janela. Guarda o handle como número para poder passar entre threads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Monitor(isize),
    Window(isize),
}

impl Target {
    /// Texto usado pela interface para identificar a opção ("m:123" / "w:456").
    pub fn id(&self) -> String {
        match self {
            Self::Monitor(h) => format!("m:{h}"),
            Self::Window(h) => format!("w:{h}"),
        }
    }

    pub fn parse(id: &str) -> Option<Self> {
        let (kind, n) = id.split_once(':')?;
        let n: isize = n.parse().ok()?;
        match kind {
            "m" => Some(Self::Monitor(n)),
            "w" => Some(Self::Window(n)),
            _ => None,
        }
    }

    /// Processo dono da janela (para transmitir só o som dele).
    pub fn process_id(&self) -> Option<u32> {
        let Self::Window(h) = self else { return None };
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd(*h), Some(&mut pid)) };
        (pid != 0).then_some(pid)
    }
}

fn hwnd(h: isize) -> HWND {
    HWND(h as *mut _)
}

fn hmonitor(h: isize) -> HMONITOR {
    HMONITOR(h as *mut _)
}

pub struct Source {
    pub target: Target,
    pub name: String,
    pub primary: bool,
}

/// Monitores (o principal primeiro) e janelas abertas, na ordem em que aparecem na tela.
pub fn list_sources() -> Vec<Source> {
    let mut out = monitors();
    out.extend(windows());
    out
}

fn monitor_info(m: HMONITOR) -> Option<MONITORINFOEXW> {
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    unsafe { GetMonitorInfoW(m, &mut info as *mut _ as *mut MONITORINFO) }.as_bool().then_some(info)
}

fn monitors() -> Vec<Source> {
    unsafe extern "system" fn each(m: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        let list = &mut *(data.0 as *mut Vec<HMONITOR>);
        list.push(m);
        true.into()
    }
    let mut handles: Vec<HMONITOR> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(each), LPARAM(&mut handles as *mut _ as isize));
    }
    let mut out: Vec<Source> = handles
        .into_iter()
        .filter_map(|m| {
            let info = monitor_info(m)?;
            let r = info.monitorInfo.rcMonitor;
            let primary = info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0;
            Some((m, r, primary))
        })
        .enumerate()
        .map(|(i, (m, r, primary))| {
            let (w, h) = (r.right - r.left, r.bottom - r.top);
            let name = if primary { format!("Tela {} (principal) · {w}×{h}", i + 1) } else { format!("Tela {} · {w}×{h}", i + 1) };
            Source { target: Target::Monitor(m.0 as isize), name, primary }
        })
        .collect();
    out.sort_by_key(|s| !s.primary);
    out
}

/// Janelas que fazem sentido transmitir: visíveis, com título, de outros programas, que não sejam
/// ferramentas/pop-ups nem janelas "escondidas" do Windows 11 (apps da loja suspensos).
fn windows() -> Vec<Source> {
    unsafe extern "system" fn each(w: HWND, data: LPARAM) -> BOOL {
        let list = &mut *(data.0 as *mut Vec<HWND>);
        list.push(w);
        true.into()
    }
    let mut handles: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(each), LPARAM(&mut handles as *mut _ as isize));
    }
    let me = unsafe { GetCurrentProcessId() };
    handles
        .into_iter()
        .filter(|&w| unsafe {
            if !IsWindowVisible(w).as_bool() || GetWindow(w, GW_OWNER).is_ok_and(|o| !o.is_invalid()) {
                return false;
            }
            let ex = GetWindowLongW(w, GWL_EXSTYLE) as u32;
            if ex & WS_EX_TOOLWINDOW.0 != 0 {
                return false;
            }
            let mut cloaked = 0u32;
            let _ = DwmGetWindowAttribute(w, DWMWA_CLOAKED, &mut cloaked as *mut _ as *mut _, 4);
            if cloaked != 0 {
                return false;
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(w, Some(&mut pid));
            pid != me
        })
        .filter_map(|w| {
            let len = unsafe { GetWindowTextLengthW(w) };
            if len <= 0 {
                return None;
            }
            let mut buf = vec![0u16; len as usize + 1];
            let n = unsafe { GetWindowTextW(w, &mut buf) };
            let name = String::from_utf16_lossy(&buf[..n as usize]).trim().to_string();
            if name.is_empty() || name == "Program Manager" {
                return None;
            }
            Some(Source { target: Target::Window(w.0 as isize), name, primary: false })
        })
        .collect()
}

/// Miniatura RGBA (cabe em `max_w`×`max_h`, mantendo a proporção). Usa GDI, que é bem mais
/// simples que a captura por GPU para uma foto só. Pode vir preta em jogos em tela cheia.
pub fn thumbnail(target: Target, max_w: u32, max_h: u32) -> Option<(u32, u32, Vec<u8>)> {
    unsafe {
        let (src_w, src_h) = match target {
            Target::Monitor(m) => {
                let r = monitor_info(hmonitor(m))?.monitorInfo.rcMonitor;
                (r.right - r.left, r.bottom - r.top)
            }
            Target::Window(h) => {
                let w = hwnd(h);
                // Janela minimizada ou travada: sem miniatura (PrintWindow poderia travar junto).
                if IsIconic(w).as_bool() || IsHungAppWindow(w).as_bool() {
                    return None;
                }
                let mut r = RECT::default();
                GetWindowRect(w, &mut r).ok()?;
                (r.right - r.left, r.bottom - r.top)
            }
        };
        if src_w <= 0 || src_h <= 0 {
            return None;
        }
        let scale = (max_w as f64 / src_w as f64).min(max_h as f64 / src_h as f64).min(1.0);
        let (tw, th) = (((src_w as f64 * scale) as i32).max(1), ((src_h as f64 * scale) as i32).max(1));

        let screen = GetDC(None);
        let dst_dc = CreateCompatibleDC(Some(screen));
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: tw,
                biHeight: -th, // de cima para baixo
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let result = (|| {
            let dib = CreateDIBSection(Some(dst_dc), &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
            let old_dst = SelectObject(dst_dc, dib.into());
            SetStretchBltMode(dst_dc, HALFTONE);
            let ok = match target {
                Target::Monitor(m) => {
                    let r = monitor_info(hmonitor(m))?.monitorInfo.rcMonitor;
                    StretchBlt(dst_dc, 0, 0, tw, th, Some(screen), r.left, r.top, src_w, src_h, SRCCOPY).as_bool()
                }
                Target::Window(h) => {
                    // A janela se desenha num bitmap do tamanho dela; depois reduzimos.
                    let src_dc = CreateCompatibleDC(Some(screen));
                    let full = CreateCompatibleBitmap(screen, src_w, src_h);
                    let old_src = SelectObject(src_dc, full.into());
                    // 2 = PW_RENDERFULLCONTENT: funciona com janelas aceleradas (Chrome, apps novos).
                    let printed = PrintWindow(hwnd(h), src_dc, PRINT_WINDOW_FLAGS(2)).as_bool();
                    let ok = printed && StretchBlt(dst_dc, 0, 0, tw, th, Some(src_dc), 0, 0, src_w, src_h, SRCCOPY).as_bool();
                    SelectObject(src_dc, old_src);
                    let _ = DeleteObject(full.into());
                    let _ = DeleteDC(src_dc);
                    ok
                }
            };
            let pixels = ok.then(|| {
                let bgra = std::slice::from_raw_parts(bits as *const u8, (tw * th * 4) as usize);
                let mut rgba = Vec::with_capacity(bgra.len());
                for px in bgra.chunks_exact(4) {
                    rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
                }
                rgba
            });
            SelectObject(dst_dc, old_dst);
            let _ = DeleteObject(dib.into());
            pixels
        })();
        let _ = DeleteDC(dst_dc);
        ReleaseDC(None, screen);
        result.map(|p| (tw as u32, th as u32, p))
    }
}

// ---------------------------------------------------------------- captura

pub struct ScreenCapture {
    d3d: IDirect3DDevice,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    /// Tamanho inicial do conteúdo (define a resolução da live).
    pub width: u32,
    pub height: u32,
    /// Tamanho da textura de trabalho: numa janela, o do monitor dela (a janela pode crescer).
    pub buffer_width: u32,
    pub buffer_height: u32,
    /// Tamanho atual do pool (muda quando a janela é redimensionada).
    pool_size: Cell<SizeInt32>,
    closed: Arc<AtomicBool>,
}

impl ScreenCapture {
    pub fn new(gpu: &Gpu, target: Target) -> Result<Self> {
        let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let (item, buffer): (GraphicsCaptureItem, Option<(u32, u32)>) = match target {
            Target::Monitor(m) => {
                let item = unsafe { interop.CreateForMonitor(hmonitor(m)) }.context("não consegui capturar o monitor")?;
                (item, None)
            }
            Target::Window(h) => {
                let w = hwnd(h);
                if !unsafe { IsWindow(Some(w)) }.as_bool() {
                    bail!("essa janela foi fechada");
                }
                // Minimizada não desenha nada: traz de volta.
                if unsafe { IsIconic(w) }.as_bool() {
                    let _ = unsafe { ShowWindow(w, SW_RESTORE) };
                }
                let item = unsafe { interop.CreateForWindow(w) }.context("não consegui capturar essa janela")?;
                let monitor = unsafe { MonitorFromWindow(w, MONITOR_DEFAULTTONEAREST) };
                let r = monitor_info(monitor).map(|i| i.monitorInfo.rcMonitor).unwrap_or_default();
                (item, Some(((r.right - r.left).max(0) as u32, (r.bottom - r.top).max(0) as u32)))
            }
        };
        let size = item.Size()?;
        let (width, height) = (size.Width.max(2) as u32, size.Height.max(2) as u32);
        let (buffer_width, buffer_height) = match buffer {
            Some((bw, bh)) => (bw.max(width), bh.max(height)),
            None => (width, height),
        };

        let closed = Arc::new(AtomicBool::new(false));
        {
            let closed = closed.clone();
            item.Closed(&TypedEventHandler::new(move |_, _| {
                closed.store(true, Ordering::Relaxed);
                Ok(())
            }))?;
        }

        // O WinRT precisa do mesmo device D3D11 embrulhado como IDirect3DDevice.
        let dxgi: IDXGIDevice = gpu.device.cast()?;
        let d3d: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? }.cast()?;

        // 2 buffers bastam: sempre pegamos só o quadro mais recente.
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&d3d, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, size)?;
        let session = pool.CreateCaptureSession(&item)?;
        let _ = session.SetIsBorderRequired(false); // tira a borda amarela (Win11)
        let _ = session.SetIsCursorCaptureEnabled(true);
        session.StartCapture()?;

        Ok(Self { d3d, pool, session, width, height, buffer_width, buffer_height, pool_size: Cell::new(size), closed })
    }

    /// A monitor principal (usado pelo protótipo de linha de comando).
    pub fn primary_monitor(gpu: &Gpu) -> Result<Self> {
        let monitor = unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) };
        Self::new(gpu, Target::Monitor(monitor.0 as isize))
    }

    /// A janela capturada foi fechada.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    /// Descarta quadros antigos e devolve o mais novo (se a tela mudou desde a última vez),
    /// junto com o tamanho útil dele. O WGC só entrega quadro quando algo muda, então `None`
    /// é normal em tela parada.
    pub fn latest(&self) -> Result<Option<(ID3D11Texture2D, u32, u32)>> {
        let mut last = None;
        while let Ok(frame) = self.pool.TryGetNextFrame() {
            last = Some(frame);
        }
        let Some(frame) = last else { return Ok(None) };
        let content = frame.ContentSize()?;
        // Janela mudou de tamanho: o pool passa a ter o tamanho novo a partir do próximo quadro.
        if content != self.pool_size.get() && content.Width > 0 && content.Height > 0 {
            self.pool.Recreate(&self.d3d, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, content)?;
            self.pool_size.set(content);
        }
        let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
        let tex: ID3D11Texture2D = unsafe { access.GetInterface()? };
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.GetDesc(&mut desc) };
        let w = (content.Width.max(1) as u32).min(desc.Width);
        let h = (content.Height.max(1) as u32).min(desc.Height);
        Ok(Some((tex, w, h)))
    }
}

/// Região (0,0)–(w,h) de uma textura, para copiar só a parte útil do quadro.
pub(crate) fn region(w: u32, h: u32) -> D3D11_BOX {
    D3D11_BOX { left: 0, top: 0, front: 0, right: w, bottom: h, back: 1 }
}

impl Drop for ScreenCapture {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}
