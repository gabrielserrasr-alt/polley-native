//! Encoder H.264 das placas NVIDIA falando direto com o driver (NVENC), sem o Media Foundation.
//! A parte que conversa com a API da NVIDIA está em `nvenc/bridge.cpp` (compilada pelo build.rs).

use anyhow::{bail, Result};
use std::ffi::{c_char, c_int, c_void, CStr};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};

use crate::encoder::{EncodedFrame, EncoderControl};

#[repr(C)]
struct NvBridge {
    _private: [u8; 0],
}

extern "C" {
    fn nvb_open(
        device: *mut c_void,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        out: *mut *mut NvBridge,
        err: *mut c_char,
        err_len: c_int,
    ) -> c_int;
    fn nvb_encode(
        b: *mut NvBridge,
        texture: *mut c_void,
        timestamp: u64,
        force_idr: c_int,
        data: *mut *const u8,
        len: *mut u32,
        keyframe: *mut c_int,
    ) -> c_int;
    fn nvb_release(b: *mut NvBridge);
    fn nvb_set_bitrate(b: *mut NvBridge, bitrate: u32) -> c_int;
    fn nvb_close(b: *mut NvBridge);
}

/// Pedidos que chegam de outra parte do laço (keyframe, bitrate) e são aplicados no próximo quadro.
#[derive(Default)]
pub struct Requests {
    pub keyframe: AtomicBool,
    /// 0 = sem pedido.
    pub bitrate: AtomicU32,
}

pub struct Nvenc {
    bridge: *mut NvBridge,
    fps: u32,
    requests: Arc<Requests>,
    pub name: String,
    /// Mantém o device vivo enquanto a sessão do NVENC existir.
    _device: ID3D11Device,
}

// A sessão é usada só pela thread de vídeo que a criou (o Stream a move para lá uma vez).
unsafe impl Send for Nvenc {}

impl Nvenc {
    pub fn new(device: &ID3D11Device, width: u32, height: u32, fps: u32, bitrate: u32) -> Result<Self> {
        let mut bridge = std::ptr::null_mut();
        let mut err = [0 as c_char; 256];
        let status =
            unsafe { nvb_open(device.as_raw(), width, height, fps, bitrate, &mut bridge, err.as_mut_ptr(), err.len() as c_int) };
        if status != 0 || bridge.is_null() {
            let msg = unsafe { CStr::from_ptr(err.as_ptr()) }.to_string_lossy().into_owned();
            bail!("{}", if msg.is_empty() { format!("NVENC falhou (código {status})") } else { msg });
        }
        Ok(Self {
            bridge,
            fps,
            requests: Arc::default(),
            name: "NVIDIA NVENC H.264 (direto)".into(),
            _device: device.clone(),
        })
    }

    pub fn control(&self) -> EncoderControl {
        EncoderControl::Nvenc(self.requests.clone())
    }

    /// Mesmo contrato do [`crate::encoder::Encoder::run`]: `next_input(n)` dita o ritmo e devolve
    /// a textura NV12 do quadro n; `on_output` recebe cada quadro comprimido.
    pub fn run(
        &mut self,
        stop: &AtomicBool,
        mut next_input: impl FnMut(u64) -> Result<ID3D11Texture2D>,
        mut on_output: impl FnMut(EncodedFrame) -> Result<()>,
    ) -> Result<()> {
        let mut n: u64 = 0;
        while !stop.load(Ordering::Relaxed) {
            let texture = next_input(n)?;
            let bitrate = self.requests.bitrate.swap(0, Ordering::Relaxed);
            if bitrate != 0 {
                unsafe { nvb_set_bitrate(self.bridge, bitrate) };
            }
            let force_idr = self.requests.keyframe.swap(false, Ordering::Relaxed);

            let timestamp_us = (n * 1_000_000 / self.fps as u64) as i64;
            let (mut data, mut len, mut keyframe) = (std::ptr::null(), 0u32, 0 as c_int);
            let status = unsafe {
                nvb_encode(self.bridge, texture.as_raw(), n, force_idr as c_int, &mut data, &mut len, &mut keyframe)
            };
            if status != 0 {
                unsafe { nvb_release(self.bridge) };
                bail!("NVENC parou de codificar (código {status})");
            }
            let result = if len > 0 {
                let bytes = unsafe { std::slice::from_raw_parts(data, len as usize) };
                on_output(EncodedFrame { data: bytes, keyframe: keyframe != 0, timestamp_us })
            } else {
                Ok(())
            };
            unsafe { nvb_release(self.bridge) };
            result?;
            n += 1;
        }
        Ok(())
    }
}

impl Drop for Nvenc {
    fn drop(&mut self) {
        unsafe { nvb_close(self.bridge) };
    }
}
