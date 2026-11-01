//! Dispositivo Direct3D 11 compartilhado por captura, conversão e encoder.
//! Tudo roda na mesma GPU, então as texturas nunca precisam passar pela RAM.

use anyhow::{Context, Result};
use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};

pub struct Gpu {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub name: String,
    /// Identificador da placa (usado para escolher o encoder DESSA placa).
    pub luid: u64,
}

pub struct Adapter {
    pub adapter: IDXGIAdapter1,
    pub name: String,
}

/// Placas de vídeo reais (ignora a "Microsoft Basic Render" e adaptadores virtuais tipo Parsec).
/// Ordem: dedicadas (NVIDIA/AMD) primeiro, integrada (Intel) depois.
pub fn adapters() -> Result<Vec<Adapter>> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    let mut out = Vec::new();
    let mut i = 0;
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(i) } {
        i += 1;
        let desc = unsafe { adapter.GetDesc1()? };
        let len = desc.Description.iter().position(|&c| c == 0).unwrap_or(desc.Description.len());
        let name = String::from_utf16_lossy(&desc.Description[..len]);
        let virtual_or_soft = desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0
            || name.contains("Basic Render")
            || name.contains("Parsec")
            || name.contains("Virtual");
        if !virtual_or_soft {
            out.push(Adapter { adapter, name });
        }
    }
    out.sort_by_key(|a| if a.name.contains("NVIDIA") || a.name.contains("AMD") || a.name.contains("Radeon") { 0 } else { 1 });
    Ok(out)
}

impl Gpu {
    pub fn new(adapter: &Adapter) -> Result<Self> {
        let mut device = None;
        let mut context = None;
        unsafe {
            D3D11CreateDevice(
                &adapter.adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                // VIDEO_SUPPORT: necessário para o Video Processor e para o encoder de hardware.
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .context("D3D11CreateDevice falhou (placa de vídeo sem suporte a vídeo?)")?;
        }
        let device: ID3D11Device = device.context("sem device")?;
        let context: ID3D11DeviceContext = context.context("sem context")?;
        // O encoder do Media Foundation usa o mesmo device em outra thread.
        let _ = unsafe { device.cast::<ID3D11Multithread>()?.SetMultithreadProtected(true) };
        let luid = unsafe { adapter.adapter.GetDesc1()?.AdapterLuid };
        let luid = ((luid.HighPart as u32 as u64) << 32) | luid.LowPart as u64;
        Ok(Self { device, context, name: adapter.name.clone(), luid })
    }

    /// Cria uma textura só na GPU (USAGE_DEFAULT, sem acesso da CPU).
    pub fn texture(&self, width: u32, height: u32, format: DXGI_FORMAT) -> Result<ID3D11Texture2D> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut tex = None;
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut tex))? };
        tex.context("CreateTexture2D retornou vazio")
    }
}
