//! "Pegar cores da imagem": monta o tema do perfil (principal, destaque, fundo, texto) a partir de uma foto.
//!
//! Como funciona:
//! 1. reduz a imagem para 64x64 (4096 pixels bastam para achar as cores dominantes);
//! 2. agrupa cores parecidas em "baldes" (32 níveis por canal) e conta quantos pixels caem em cada um;
//! 3. dá uma nota para cada balde: presença na imagem × vivacidade (cores acinzentadas, quase pretas
//!    ou quase brancas perdem pontos — senão a cor principal de quase toda foto seria "cinza");
//! 4. principal = maior nota; destaque = maior nota com matiz bem diferente (≥ 40° no círculo de cores);
//! 5. fundo = a principal bem escura; texto = branco ou preto, o que tiver mais contraste com o fundo.

use image::imageops::FilterType;
use image::DynamicImage;
use std::collections::HashMap;

#[derive(Clone, Debug)]
pub struct Theme {
    pub primary: String,
    pub accent: String,
    pub background: String,
    pub text: String,
}

/// Nome de cada cor do tema, na ordem em que aparecem no editor.
pub const SLOTS: [(&str, &str); 4] =
    [("primary", "Cor principal"), ("accent", "Cor de destaque"), ("background", "Fundo do card"), ("text", "Texto")];

/// Temas prontos (os mesmos do app Electron).
pub const PRESETS: [[&str; 4]; 6] = [
    ["#5865f2", "#eb459e", "#232428", "#f2f3f5"],
    ["#23a55a", "#f0b232", "#10261a", "#f2f3f5"],
    ["#f23f43", "#ff9f43", "#2a1215", "#fff5f5"],
    ["#00b0f4", "#9b59b6", "#0d1b2a", "#e8f6ff"],
    ["#f5c2e7", "#cba6f7", "#1e1e2e", "#cdd6f4"],
    ["#f2f3f5", "#80848e", "#e3e5e8", "#060607"],
];

impl Theme {
    pub fn from_array(c: [&str; 4]) -> Self {
        Theme { primary: c[0].into(), accent: c[1].into(), background: c[2].into(), text: c[3].into() }
    }

    /// Lê o `theme` do perfil (jsonb); cor faltando = a do tema padrão.
    pub fn from_json(v: &serde_json::Value) -> Self {
        let d = PRESETS[0];
        let get = |k: &str, i: usize| normalize_hex(v[k].as_str().unwrap_or(d[i])).unwrap_or_else(|| d[i].into());
        Theme { primary: get("primary", 0), accent: get("accent", 1), background: get("background", 2), text: get("text", 3) }
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({ "primary": self.primary, "accent": self.accent, "background": self.background, "text": self.text })
    }

    pub fn as_array(&self) -> [&str; 4] {
        [&self.primary, &self.accent, &self.background, &self.text]
    }

    pub fn set(&mut self, index: usize, hex: String) {
        match index {
            0 => self.primary = hex,
            1 => self.accent = hex,
            2 => self.background = hex,
            _ => self.text = hex,
        }
    }
}

/// Aceita "#abc", "abc", "#aabbcc" ou "aabbcc" e devolve "#aabbcc" (minúsculo). Inválido = `None`.
pub fn normalize_hex(s: &str) -> Option<String> {
    let h = s.trim().trim_start_matches('#').to_lowercase();
    if !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    match h.len() {
        6 => Some(format!("#{h}")),
        3 => Some(format!("#{}", h.chars().flat_map(|c| [c, c]).collect::<String>())),
        _ => None,
    }
}

type Rgb = [f32; 3];

fn hex([r, g, b]: Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", r.round() as u8, g.round() as u8, b.round() as u8)
}

/// RGB (0–255) → HSL (matiz 0–360, saturação 0–1, luminosidade 0–1).
fn to_hsl([r, g, b]: Rgb) -> (f32, f32, f32) {
    let (r, g, b) = (r / 255.0, g / 255.0, b / 255.0);
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    let l = (max + min) / 2.0;
    if max == min {
        return (0.0, 0.0, l);
    }
    let d = max - min;
    let s = if l > 0.5 { d / (2.0 - max - min) } else { d / (max + min) };
    let h = if max == r {
        (g - b) / d + if g < b { 6.0 } else { 0.0 }
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    (h * 60.0, s, l)
}

fn from_hsl(h: f32, s: f32, l: f32) -> Rgb {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = match h as u32 {
        0..=59 => (c, x, 0.0),
        60..=119 => (x, c, 0.0),
        120..=179 => (0.0, c, x),
        180..=239 => (0.0, x, c),
        240..=299 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    [(r + m) * 255.0, (g + m) * 255.0, (b + m) * 255.0]
}

/// Luminância relativa (WCAG): 0 = preto, 1 = branco.
fn luminance(c: Rgb) -> f32 {
    let lin = |v: f32| {
        let s = v / 255.0;
        if s <= 0.03928 { s / 12.92 } else { ((s + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * lin(c[0]) + 0.7152 * lin(c[1]) + 0.0722 * lin(c[2])
}

/// Contraste WCAG (1 a 21; texto confortável ≥ 4,5).
fn contrast(a: Rgb, b: Rgb) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// Distância entre dois matizes no círculo de cores (0–180°).
fn hue_distance(a: f32, b: f32) -> f32 {
    let d = (a - b).abs() % 360.0;
    d.min(360.0 - d)
}

struct Bucket {
    color: Rgb,
    count: u32,
}

pub fn extract(img: &DynamicImage) -> Theme {
    let small = img.resize_exact(64, 64, FilterType::Triangle).to_rgba8();

    // 1–2. baldes de cores parecidas
    let mut sums: HashMap<u16, ([u64; 3], u32)> = HashMap::new();
    for px in small.pixels() {
        if px[3] < 128 {
            continue; // transparente
        }
        let key = ((px[0] as u16 >> 3) << 10) | ((px[1] as u16 >> 3) << 5) | (px[2] as u16 >> 3);
        let e = sums.entry(key).or_insert(([0; 3], 0));
        for i in 0..3 {
            e.0[i] += px[i] as u64;
        }
        e.1 += 1;
    }
    let total: u32 = sums.values().map(|e| e.1).sum::<u32>().max(1);
    let buckets: Vec<Bucket> = sums
        .values()
        .map(|(s, n)| Bucket { color: [s[0] as f32 / *n as f32, s[1] as f32 / *n as f32, s[2] as f32 / *n as f32], count: *n })
        .collect();
    if buckets.is_empty() {
        return Theme { primary: "#5865f2".into(), accent: "#eb459e".into(), background: "#232428".into(), text: "#f2f3f5".into() };
    }

    // 3. nota: presença × vivacidade
    let score = |b: &Bucket| {
        let (_, s, l) = to_hsl(b.color);
        let presence = (b.count as f32 / total as f32).sqrt(); // raiz: uma cor pequena e viva ainda compete
        let extremes = if !(0.12..=0.92).contains(&l) { 0.3 } else { 1.0 };
        presence * (0.25 + s) * extremes
    };

    // 4. principal e destaque
    let primary = buckets.iter().max_by(|a, b| score(a).total_cmp(&score(b))).unwrap();
    let (ph, ps, pl) = to_hsl(primary.color);
    let accent = buckets
        .iter()
        .filter(|b| {
            let (h, s, _) = to_hsl(b.color);
            s > 0.2 && hue_distance(h, ph) >= 40.0
        })
        .max_by(|a, b| score(a).total_cmp(&score(b)))
        .map(|b| b.color)
        // Imagem de uma cor só: o destaque vira a principal mais clara (ou mais escura).
        .unwrap_or_else(|| from_hsl(ph, ps, if pl < 0.5 { (pl + 0.25).min(0.85) } else { (pl - 0.25).max(0.2) }));

    // 5. fundo escuro puxado para a principal + texto legível
    let background = from_hsl(ph, ps.min(0.45), 0.13);
    let white = [255.0, 255.0, 255.0];
    let dark = [6.0, 6.0, 7.0];
    let text = if contrast(white, background) >= contrast(dark, background) { [242.0, 243.0, 245.0] } else { dark };

    Theme { primary: hex(primary.color), accent: hex(accent), background: hex(background), text: hex(text) }
}
