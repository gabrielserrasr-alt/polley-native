//! Motor de mídia do Polley (Windows).
//!
//! Pipeline de vídeo — a imagem nunca sai da placa de vídeo até estar comprimida:
//!   tela → [`capture`] (WGC, textura GPU) → [`convert`] (NV12 + escala, GPU)
//!        → [`nvenc`] (placas NVIDIA, direto no driver) ou [`encoder`] (Media Foundation: Intel/AMD)
//!        → bytes H.264 para o LiveKit
//!
//! Áudio: [`audio`] (microfone com cancelamento de eco + som do PC sem o próprio app).

pub mod audio;
pub mod camera;
pub mod camstream;
pub mod capture;
pub mod convert;
pub mod encoder;
pub mod gpu;
pub mod mem;
pub mod nvenc;
pub mod mictest;
pub mod playback;
pub mod record;
pub mod stream;
pub mod videoview;
