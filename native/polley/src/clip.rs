//! Área de transferência do Windows (o que o Ctrl+V cola). Usada pelo "Copiar ID" do modo
//! desenvolvedor e pelo código da verificação em 2 etapas.

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_UNICODETEXT;

/// Copia `text`. Devolve `false` se outro programa estava usando a área de transferência.
pub fn set_text(text: &str) -> bool {
    let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    unsafe {
        if OpenClipboard(None).is_err() {
            return false;
        }
        let ok = (|| -> windows::core::Result<()> {
            EmptyClipboard()?;
            let mem = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2)?;
            let ptr = GlobalLock(mem) as *mut u16;
            if ptr.is_null() {
                return Err(windows::core::Error::from_thread());
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
            let _ = GlobalUnlock(mem);
            // Depois do SetClipboardData, a memória é do Windows (não liberar).
            SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(mem.0)))?;
            Ok(())
        })()
        .is_ok();
        let _ = CloseClipboard();
        ok
    }
}
