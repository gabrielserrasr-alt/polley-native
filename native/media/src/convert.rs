//! Converte BGRA (tela) → NV12 (o que o encoder H.264 aceita) e redimensiona, tudo na GPU,
//! usando o Video Processor do D3D11 (o mesmo bloco de hardware que reproduz vídeos).

use anyhow::{Context, Result};
use std::cell::Cell;
use std::mem::ManuallyDrop;
use windows::core::Interface;
use windows::Win32::Foundation::{RECT, TRUE};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL};

use crate::gpu::Gpu;

/// Quantas texturas NV12 em rodízio: o encoder pode estar lendo uma enquanto escrevemos a próxima.
/// Ficam na memória da placa de vídeo (VRAM), não na RAM.
const RING: usize = 3;

pub struct Converter {
    context: ID3D11DeviceContext,
    video_context: ID3D11VideoContext,
    processor: ID3D11VideoProcessor,
    /// Cópia estável do último quadro da tela (o WGC recicla as texturas dele).
    pub bgra: ID3D11Texture2D,
    input_view: ID3D11VideoProcessorInputView,
    outputs: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    buffer: (u32, u32),
    out: (u32, u32),
    /// Tamanho útil do último quadro (numa janela, muda quando ela é redimensionada).
    content: Cell<(u32, u32)>,
}

impl Converter {
    pub fn new(gpu: &Gpu, in_w: u32, in_h: u32, out_w: u32, out_h: u32, fps: u32) -> Result<Self> {
        let video_device: ID3D11VideoDevice = gpu.device.cast()?;
        let video_context: ID3D11VideoContext = gpu.context.cast()?;
        let rate = DXGI_RATIONAL { Numerator: fps, Denominator: 1 };
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: in_w,
            InputHeight: in_h,
            OutputFrameRate: rate,
            OutputWidth: out_w,
            OutputHeight: out_h,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&desc)? };
        let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0)? };

        let bgra = gpu.texture(in_w, in_h, DXGI_FORMAT_B8G8R8A8_UNORM)?;
        let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 } },
        };
        let mut input_view = None;
        unsafe { video_device.CreateVideoProcessorInputView(&bgra, &enumerator, &in_desc, Some(&mut input_view))? };

        let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
        };
        let mut outputs = Vec::with_capacity(RING);
        for _ in 0..RING {
            let tex = gpu.texture(out_w, out_h, DXGI_FORMAT_NV12)?;
            let mut view = None;
            unsafe { video_device.CreateVideoProcessorOutputView(&tex, &enumerator, &out_desc, Some(&mut view))? };
            outputs.push((tex, view.context("output view vazia")?));
        }

        unsafe {
            // Entrada: RGB 0–255. Saída: YCbCr BT.709 com faixa 16–235 (padrão de vídeo).
            // Bits do D3D11_VIDEO_PROCESSOR_COLOR_SPACE: Usage|RGB_Range|YCbCr_Matrix|xvYCC|Nominal_Range(2)
            video_context.VideoProcessorSetStreamColorSpace(&processor, 0, &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 });
            video_context.VideoProcessorSetOutputColorSpace(&processor, &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: (1 << 2) | (1 << 4) });
            video_context.VideoProcessorSetStreamFrameFormat(&processor, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            video_context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            let full_out = RECT { left: 0, top: 0, right: out_w as i32, bottom: out_h as i32 };
            video_context.VideoProcessorSetOutputTargetRect(&processor, true, Some(&full_out));
            // Faixas pretas quando a janela não tem a proporção da live.
            let black = D3D11_VIDEO_COLOR { Anonymous: D3D11_VIDEO_COLOR_0 { RGBA: D3D11_VIDEO_COLOR_RGBA { R: 0.0, G: 0.0, B: 0.0, A: 1.0 } } };
            video_context.VideoProcessorSetOutputBackgroundColor(&processor, false, &black);
        }

        Ok(Self {
            context: gpu.context.clone(),
            video_context,
            processor,
            bgra,
            input_view: input_view.context("input view vazia")?,
            outputs,
            buffer: (in_w, in_h),
            out: (out_w, out_h),
            content: Cell::new((0, 0)),
        })
    }

    /// Guarda uma cópia da parte útil (`w`×`h`) do quadro da tela (cópia GPU→GPU, instantânea).
    pub fn update_source(&self, frame: &ID3D11Texture2D, w: u32, h: u32) {
        let (w, h) = (w.min(self.buffer.0).max(1), h.min(self.buffer.1).max(1));
        let region = crate::capture::region(w, h);
        unsafe { self.context.CopySubresourceRegion(&self.bgra, 0, 0, 0, 0, frame, 0, Some(&region)) };
        if self.content.get() != (w, h) {
            self.content.set((w, h));
            self.fit(w, h);
        }
    }

    /// Encaixa o conteúdo na live mantendo a proporção (centralizado).
    fn fit(&self, w: u32, h: u32) {
        let (ow, oh) = (self.out.0 as f64, self.out.1 as f64);
        let scale = (ow / w as f64).min(oh / h as f64);
        let (dw, dh) = ((w as f64 * scale).round(), (h as f64 * scale).round());
        let (x, y) = (((ow - dw) / 2.0).round() as i32, ((oh - dh) / 2.0).round() as i32);
        let src = RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 };
        let dst = RECT { left: x, top: y, right: x + dw as i32, bottom: y + dh as i32 };
        unsafe {
            self.video_context.VideoProcessorSetStreamSourceRect(&self.processor, 0, true, Some(&src));
            self.video_context.VideoProcessorSetStreamDestRect(&self.processor, 0, true, Some(&dst));
        }
    }

    /// Converte o último quadro para NV12 na textura `slot` do rodízio e a devolve.
    pub fn convert(&self, slot: usize) -> Result<ID3D11Texture2D> {
        let (tex, view) = &self.outputs[slot % RING];
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: TRUE,
            pInputSurface: ManuallyDrop::new(Some(self.input_view.clone())),
            ..Default::default()
        };
        let streams = [stream];
        let result = unsafe { self.video_context.VideoProcessorBlt(&self.processor, view, 0, &streams) };
        // Soltar a referência que colocamos no ManuallyDrop.
        let [mut stream] = streams;
        unsafe { ManuallyDrop::drop(&mut stream.pInputSurface) };
        result.context("VideoProcessorBlt falhou")?;
        Ok(tex.clone())
    }
}
