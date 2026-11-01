//! Fotos de perfil, banners e imagens anexadas no chat.
//!
//! O Slint não deixa uma `Image` passar entre threads, então:
//!   - a rede e a decodificação rodam no tokio e produzem pixels (`SharedPixelBuffer`, que pode viajar);
//!   - na thread da interface os pixels viram `Image` e ficam num cache por URL;
//!   - a interface pede a imagem com `Avatars.lookup(url, Avatars.version)`; quando uma foto nova
//!     chega, `version` aumenta e todo mundo que mostra avatar se redesenha.
//!
//! Tudo é reduzido antes de guardar (avatar 128x128, banner 600x212): uma foto de 12 MP
//! do celular vira ~64 KB de RAM em vez de ~48 MB.

use anyhow::Result;
use image::imageops::FilterType;
use image::{DynamicImage, RgbaImage};
use slint::{ComponentHandle, Image, Rgba8Pixel, SharedPixelBuffer};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use crate::{AppWindow, Avatars};

/// Download máximo de uma foto (as enviadas pelo app têm poucas centenas de KB).
const MAX_DOWNLOAD: usize = 8 * 1024 * 1024;

/// Só baixa imagens do NOSSO Storage. O `avatar_url` é escrito pelo próprio usuário, então sem isso
/// alguém poderia apontar a foto para um site dele e descobrir o IP de todo mundo que a visse.
pub fn allowed(url: &str) -> bool {
    let base = format!("{}/storage/v1/object/public/", crate::supabase::URL);
    ["avatars/", "banners/", "attachments/"].iter().any(|bucket| url.starts_with(&format!("{base}{bucket}")))
        && !url.contains("..")
        && !url.contains(['?', '#', '\\'])
}

/// Abre uma imagem com limites: um PNG pequeno pode dizer que tem 50000x50000 pixels
/// (uma "bomba") e fazer o app tentar reservar gigabytes de memória.
pub fn decode(bytes: &[u8]) -> Result<DynamicImage> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(300 * 1024 * 1024);
    reader.limits(limits);
    Ok(reader.decode()?)
}

#[derive(Clone, Copy)]
pub enum Kind {
    Avatar,
    Banner,
    /// Imagem enviada no chat: reduzida para caber em 400x300, sem cortar.
    Attachment,
}

impl Kind {
    /// Chave no cache: banners têm prefixo (a mesma imagem pode ser avatar e banner).
    pub fn key(self, url: &str) -> String {
        match self {
            Kind::Avatar => url.to_string(),
            Kind::Banner => format!("banner:{url}"),
            Kind::Attachment => format!("att:{url}"),
        }
    }
}

thread_local! {
    static CACHE: RefCell<HashMap<String, Image>> = RefCell::new(HashMap::new());
}

/// Liga o `Avatars.lookup` da interface ao cache.
pub fn install(ui: &AppWindow) {
    ui.global::<Avatars>().on_lookup(|key, _version| {
        CACHE.with(|c| c.borrow().get(key.as_str()).cloned().unwrap_or_default())
    });
}

/// Guarda a imagem pronta no cache (na thread da interface) e avisa a tela.
fn store(ui: &AppWindow, key: String, pixels: SharedPixelBuffer<Rgba8Pixel>) {
    CACHE.with(|c| c.borrow_mut().insert(key, Image::from_rgba8(pixels)));
    let avatars = ui.global::<Avatars>();
    avatars.set_version(avatars.get_version() + 1);
}

fn to_buffer(img: RgbaImage) -> SharedPixelBuffer<Rgba8Pixel> {
    let (w, h) = img.dimensions();
    SharedPixelBuffer::clone_from_slice(img.as_raw(), w, h)
}

/// Recorta em círculo (com borda suave) — o renderizador por software não precisa recortar depois.
fn circle(mut img: RgbaImage) -> RgbaImage {
    let (w, h) = img.dimensions();
    let (cx, cy, r) = (w as f32 / 2.0, h as f32 / 2.0, w.min(h) as f32 / 2.0);
    for (x, y, px) in img.enumerate_pixels_mut() {
        let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
        let cover = (r - d + 0.5).clamp(0.0, 1.0); // 1 dentro, 0 fora, meio-termo na borda
        px[3] = (px[3] as f32 * cover) as u8;
    }
    img
}

/// Imagem pronta para mostrar: avatar redondo 128x128 ou banner 600x212.
pub fn for_display(img: &DynamicImage, kind: Kind) -> SharedPixelBuffer<Rgba8Pixel> {
    match kind {
        Kind::Avatar => to_buffer(circle(img.resize_to_fill(128, 128, FilterType::Triangle).to_rgba8())),
        Kind::Banner => to_buffer(img.resize_to_fill(600, 212, FilterType::Triangle).to_rgba8()),
        Kind::Attachment if img.width() <= 400 && img.height() <= 300 => to_buffer(img.to_rgba8()),
        Kind::Attachment => to_buffer(img.resize(400, 300, FilterType::Triangle).to_rgba8()),
    }
}

/// Arquivo para enviar ao Storage: avatar PNG 256x256, banner JPEG 960x340, anexo JPEG até 1920
/// (leve para todo mundo baixar — o plano grátis tem 5 GB de download por mês).
pub fn for_upload(img: &DynamicImage, kind: Kind) -> Result<(Vec<u8>, &'static str, &'static str)> {
    let mut out = std::io::Cursor::new(Vec::new());
    match kind {
        Kind::Attachment => {
            let img = if img.width() > 1920 || img.height() > 1920 { img.resize(1920, 1920, FilterType::Lanczos3) } else { img.clone() };
            let rgb = DynamicImage::ImageRgb8(img.to_rgb8());
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85).encode_image(&rgb)?;
            Ok((out.into_inner(), "image/jpeg", "jpg"))
        }
        Kind::Avatar => {
            img.resize_to_fill(256, 256, FilterType::Lanczos3).write_to(&mut out, image::ImageFormat::Png)?;
            Ok((out.into_inner(), "image/png", "png"))
        }
        Kind::Banner => {
            let rgb = DynamicImage::ImageRgb8(img.resize_to_fill(960, 340, FilterType::Lanczos3).to_rgb8());
            rgb.write_to(&mut out, image::ImageFormat::Jpeg)?;
            Ok((out.into_inner(), "image/jpeg", "jpg"))
        }
    }
}

/// Baixa imagens por URL (uma vez cada) e entrega para a interface.
pub struct Loader {
    ui: slint::Weak<AppWindow>,
    http: reqwest::Client,
    requested: Mutex<HashSet<String>>,
}

impl Loader {
    pub fn new(ui: slint::Weak<AppWindow>) -> Arc<Self> {
        // Sem seguir redirecionamentos: um endereço do nosso Storage não pode levar a outro site.
        let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap_or_default();
        Arc::new(Self { ui, http, requested: Mutex::default() })
    }

    pub fn want(self: &Arc<Self>, url: Option<&str>, kind: Kind) {
        let Some(url) = url.filter(|u| allowed(u)) else { return };
        let key = kind.key(url);
        if !self.requested.lock().unwrap().insert(key.clone()) {
            return; // já pedida
        }
        let (loader, url) = (self.clone(), url.to_string());
        tokio::spawn(async move {
            let Ok(mut resp) = crate::supabase::storage_get(&loader.http, &url).send().await else { return };
            if resp.content_length().is_some_and(|n| n as usize > MAX_DOWNLOAD) {
                return;
            }
            // Lê em pedaços: o tamanho informado pelo servidor pode mentir.
            let mut bytes = Vec::new();
            while let Ok(Some(chunk)) = resp.chunk().await {
                bytes.extend_from_slice(&chunk);
                if bytes.len() > MAX_DOWNLOAD {
                    return;
                }
            }
            // Decodificar/reduzir usa CPU: fora das threads de rede.
            let decoded = tokio::task::spawn_blocking(move || {
                decode(&bytes).ok().map(|img| for_display(&img, kind))
            })
            .await;
            if let Ok(Some(pixels)) = decoded {
                let _ = loader.ui.upgrade_in_event_loop(move |ui| store(&ui, key, pixels));
            }
        });
    }

    /// Imagem que EU acabei de enviar: já está decodificada, não precisa baixar de novo.
    pub fn put(&self, url: &str, kind: Kind, pixels: SharedPixelBuffer<Rgba8Pixel>) {
        let key = kind.key(url);
        self.requested.lock().unwrap().insert(key.clone());
        let _ = self.ui.upgrade_in_event_loop(move |ui| store(&ui, key, pixels));
    }
}
