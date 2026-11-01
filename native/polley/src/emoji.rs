//! Emojis coloridos.
//!
//! O desenhista de texto do Slint (por software) só pinta letras de uma cor: um emoji no meio do
//! texto sai como um desenho chapado. Para reações, o seletor de emojis e mensagens só de emoji,
//! pedimos ao próprio Windows (Direct2D + fonte "Segoe UI Emoji", que já vem no sistema) para
//! desenhar o emoji colorido numa imagem. Cada emoji é desenhado uma vez e fica guardado.
//! Nada é baixado da internet.

use slint::{Image, Rgba8Pixel, SharedPixelBuffer};
use std::cell::RefCell;
use std::collections::HashMap;
use windows::core::w;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};

/// Tamanho da imagem (px). A interface reduz para 16-48 px; desenhar maior deixa nítido.
const SIZE: u32 = 64;

/// Emojis do seletor (os mais usados no Brasil + os clássicos de reação).
pub const PICKER: &[&str] = &[
    "😂", "🤣", "😭", "😍", "🥰", "😘", "😊", "😁", "😅", "😆", "🙂", "😉", "😎", "🤔", "😐", "🙄",
    "😬", "😴", "🥲", "😢", "😡", "🤬", "😱", "😳", "🥺", "🤡", "💀", "👻", "🤯", "🥳", "😈", "🤝",
    "👍", "👎", "👏", "🙏", "💪", "👀", "🫡", "🤙", "✌️", "👋", "🖕", "🫶", "❤️", "💔", "🔥", "✨",
    "💯", "🎉", "⭐", "✅", "❌", "⚠️", "💩", "🍕", "🍺", "☕", "⚽", "🎮", "🎧", "🏆", "🚀", "🐐",
];

struct Painter {
    d2d: ID2D1Factory,
    wic: IWICImagingFactory,
    format: IDWriteTextFormat,
}

thread_local! {
    static PAINTER: RefCell<Option<Painter>> = const { RefCell::new(None) };
    static CACHE: RefCell<HashMap<String, Image>> = RefCell::new(HashMap::new());
}

/// Imagem colorida de um emoji (vazia se não for possível desenhar). Só na thread da interface.
pub fn image(text: &str) -> Image {
    if text.is_empty() || text.len() > 32 {
        return Image::default();
    }
    if let Some(img) = CACHE.with(|c| c.borrow().get(text).cloned()) {
        return img;
    }
    let img = PAINTER
        .with(|p| {
            let mut p = p.borrow_mut();
            if p.is_none() {
                *p = unsafe { Painter::new() };
            }
            p.as_ref().and_then(|p| unsafe { p.draw(text) })
        })
        .map(Image::from_rgba8_premultiplied)
        .unwrap_or_default();
    CACHE.with(|c| c.borrow_mut().insert(text.to_string(), img.clone()));
    img
}

impl Painter {
    unsafe fn new() -> Option<Self> {
        // Normalmente o COM já está ligado nesta thread (a janela usa); se não, liga.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None).ok()?;
        let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED).ok()?;
        let wic: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
        let format = dw
            .CreateTextFormat(
                w!("Segoe UI Emoji"),
                None,
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                SIZE as f32 * 0.72,
                w!("pt-br"),
            )
            .ok()?;
        let _ = format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER);
        let _ = format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
        let _ = format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
        Some(Self { d2d, wic, format })
    }

    unsafe fn draw(&self, text: &str) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
        let bitmap = self.wic.CreateBitmap(SIZE, SIZE, &GUID_WICPixelFormat32bppPBGRA, WICBitmapCacheOnLoad).ok()?;
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
            dpiX: 96.0,
            dpiY: 96.0,
            ..Default::default()
        };
        let target = self.d2d.CreateWicBitmapRenderTarget(&bitmap, &props).ok()?;
        let brush = target.CreateSolidColorBrush(&D2D1_COLOR_F { r: 1.0, g: 1.0, b: 1.0, a: 1.0 }, None).ok()?;
        let wide: Vec<u16> = text.encode_utf16().collect();
        let rect = D2D_RECT_F { left: 0.0, top: 0.0, right: SIZE as f32, bottom: SIZE as f32 };
        target.BeginDraw();
        target.Clear(Some(&D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }));
        target.DrawText(
            &wide,
            &self.format,
            &rect,
            &brush,
            D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT | D2D1_DRAW_TEXT_OPTIONS_CLIP,
            DWRITE_MEASURING_MODE_NATURAL,
        );
        target.EndDraw(None, None).ok()?;

        let mut bytes = vec![0u8; (SIZE * SIZE * 4) as usize];
        bitmap.CopyPixels(std::ptr::null(), SIZE * 4, &mut bytes).ok()?;
        // Windows guarda BGRA; o Slint quer RGBA (os dois já com a transparência "pré-multiplicada").
        let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(SIZE, SIZE);
        for (px, bgra) in buf.make_mut_slice().iter_mut().zip(bytes.chunks_exact(4)) {
            *px = Rgba8Pixel { r: bgra[2], g: bgra[1], b: bgra[0], a: bgra[3] };
        }
        Some(buf)
    }
}

/// A mensagem é só emoji (até 27, sem texto)? Então mostramos grandes e coloridos, igual ao Discord.
/// Devolve cada emoji separado.
pub fn only_emojis(text: &str) -> Option<Vec<String>> {
    let text = text.trim();
    if text.is_empty() || text.chars().count() > 60 {
        return None;
    }
    let mut out: Vec<String> = Vec::new();
    for c in text.chars() {
        if c.is_whitespace() {
            continue;
        }
        let join = matches!(c, '\u{200D}' | '\u{FE0F}' | '\u{20E3}' | '\u{1F3FB}'..='\u{1F3FF}');
        let prev_zwj = out.last().is_some_and(|s| s.ends_with('\u{200D}'));
        if join || prev_zwj {
            // Continua o emoji anterior (tom de pele, "variação colorida", família 👨‍👩‍👧...).
            out.last_mut()?.push(c);
        } else if is_emoji(c) {
            out.push(c.to_string());
        } else {
            return None;
        }
    }
    (!out.is_empty() && out.len() <= 27).then_some(out)
}

fn is_emoji(c: char) -> bool {
    matches!(c as u32,
        0x1F300..=0x1FAFF | 0x2600..=0x27BF | 0x2B00..=0x2BFF | 0x1F000..=0x1F2FF | 0x2190..=0x21FF | 0x2300..=0x23FF | 0x3030 | 0x303D | 0x3297 | 0x3299 | 0x00A9 | 0x00AE | 0x203C | 0x2049 | 0x2122 | 0x2139)
}

#[cfg(test)]
mod tests {
    #[test]
    fn desenha_emoji_colorido() {
        let p = unsafe { super::Painter::new() }.expect("Direct2D");
        let buf = unsafe { p.draw("😂") }.expect("desenho");
        let px = buf.as_slice();
        let visiveis = px.iter().filter(|p| p.a > 200).count();
        // Colorido: tem pixel amarelo (vermelho e verde altos, azul baixo), não só branco/cinza.
        let amarelos = px.iter().filter(|p| p.a > 200 && p.r > 180 && p.g > 120 && p.b < 90).count();
        assert!(visiveis > 500, "quase nada desenhado: {visiveis}");
        assert!(amarelos > 100, "sem cor: {amarelos}");
        assert_eq!(super::only_emojis("😂😂 👍🏽"), Some(vec!["😂".into(), "😂".into(), "👍🏽".into()]));
        assert_eq!(super::only_emojis("oi 😂"), None);
    }
}
