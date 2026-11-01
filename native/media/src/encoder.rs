//! Encoder H.264 de hardware via Media Foundation (NVENC / AMD AMF / Intel QuickSync).
//! Recebe texturas NV12 que já estão na GPU e devolve só os bytes comprimidos.

use anyhow::{Context, Result};
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, Ordering};
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Variant::VARIANT;

pub struct EncodedFrame<'a> {
    pub data: &'a [u8],
    pub keyframe: bool,
    /// Tempo de apresentação em microssegundos.
    pub timestamp_us: i64,
}

/// Controles que podem ser usados de dentro do laço de encode (keyframe, bitrate).
#[derive(Clone)]
pub enum EncoderControl {
    MediaFoundation(ICodecAPI),
    Nvenc(std::sync::Arc<crate::nvenc::Requests>),
}

impl EncoderControl {
    pub fn force_keyframe(&self) {
        match self {
            Self::MediaFoundation(codec) => {
                let _ = unsafe { codec.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32)) };
            }
            Self::Nvenc(r) => r.keyframe.store(true, Ordering::Relaxed),
        }
    }
    pub fn set_bitrate(&self, bps: u32) {
        match self {
            Self::MediaFoundation(codec) => {
                let _ = unsafe { codec.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &VARIANT::from(bps)) };
            }
            Self::Nvenc(r) => r.bitrate.store(bps, Ordering::Relaxed),
        }
    }
}

pub struct Encoder {
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    control: EncoderControl,
    _manager: IMFDXGIDeviceManager,
    in_id: u32,
    out_id: u32,
    provides_samples: bool,
    fps: u32,
    /// SPS/PPS (cabeçalho H.264): precisa ir junto de todo keyframe pra quem entra no meio da live.
    sequence_header: Vec<u8>,
    pub name: String,
}

fn pack(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

impl Encoder {
    pub fn new(device: &ID3D11Device, adapter_luid: u64, width: u32, height: u32, fps: u32, bitrate: u32) -> Result<Self> {
        unsafe {
            // 1. Acha um encoder H.264 de HARDWARE que aceite NV12.
            let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
            let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
            let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
            let mut count = 0u32;
            // Com 2 placas (ex.: Intel + NVIDIA), cada encoder pertence a uma placa: o filtro
            // MFT_ENUM_ADAPTER_LUID devolve só os encoders da MESMA placa das nossas texturas.
            let mut filter = None;
            MFCreateAttributes(&mut filter, 1)?;
            let filter: IMFAttributes = filter.context("atributos vazios")?;
            let luid = [(adapter_luid as u32).to_le_bytes(), ((adapter_luid >> 32) as u32).to_le_bytes()].concat();
            filter.SetBlob(&MFT_ENUM_ADAPTER_LUID, &luid)?;
            MFTEnum2(
                MFT_CATEGORY_VIDEO_ENCODER,
                MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
                Some(&input),
                Some(&output),
                &filter,
                &mut activates,
                &mut count,
            )?;
            let list = std::slice::from_raw_parts_mut(activates, count as usize);
            let activate = list.first_mut().and_then(|a| a.take());
            for a in list.iter_mut() {
                drop(a.take());
            }
            windows::Win32::System::Com::CoTaskMemFree(Some(activates as *const _));
            let activate = activate.context("esta placa de vídeo não tem encoder H.264 de hardware")?;

            let name = {
                let mut ptr = windows::core::PWSTR::null();
                let mut len = 0u32;
                if activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut ptr, &mut len).is_ok() {
                    let s = ptr.to_string().unwrap_or_default();
                    windows::Win32::System::Com::CoTaskMemFree(Some(ptr.0 as *const _));
                    s
                } else {
                    "encoder de hardware".into()
                }
            };
            let transform: IMFTransform = activate.ActivateObject().context("ActivateObject")?;

            // 2. Encoders de hardware são assíncronos: precisam ser "destravados".
            let attrs = transform.GetAttributes()?;
            attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1).context("ASYNC_UNLOCK")?;
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);

            // 3. Entrega nosso device D3D11 pro encoder (é isso que evita cópias pra RAM).
            let mut token = 0u32;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
            let manager: IMFDXGIDeviceManager = manager.context("device manager vazio")?;
            manager.ResetDevice(device, token).context("ResetDevice")?;
            transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize).context("SET_D3D_MANAGER")?;

            // 4. Controle de taxa: CBR + baixa latência (ao vivo, não arquivo pra YouTube).
            let codec: ICodecAPI = transform.cast()?;
            let _ = codec.SetValue(&CODECAPI_AVEncCommonRateControlMode, &VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32));
            let _ = codec.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &VARIANT::from(bitrate));
            let _ = codec.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true));
            let _ = codec.SetValue(&CODECAPI_AVEncMPVGOPSize, &VARIANT::from(fps * 2)); // keyframe a cada 2s
            let _ = codec.SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &VARIANT::from(0u32)); // sem B-frames = menos atraso
            let _ = codec.SetValue(&CODECAPI_AVEncVideoMaxNumRefFrame, &VARIANT::from(1u32)); // 1 quadro de referência = menos memória

            // 5. Formatos: saída H.264 primeiro, depois entrada NV12 (ordem exigida pelos encoders).
            let (mut in_ids, mut out_ids) = ([0u32; 1], [0u32; 1]);
            let (in_id, out_id) = match transform.GetStreamIDs(&mut in_ids, &mut out_ids) {
                Ok(()) => (in_ids[0], out_ids[0]),
                Err(_) => (0, 0), // E_NOTIMPL = IDs fixos 0
            };

            let out_type = MFCreateMediaType()?;
            out_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            out_type.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            out_type.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
            out_type.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            out_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            out_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            // Main: tocado por qualquer decoder (inclusive o do Electron/Chromium).
            out_type.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32)?;
            transform.SetOutputType(out_id, &out_type, 0).context("encoder recusou o formato de saída (resolução alta demais?)")?;

            let in_type = MFCreateMediaType()?;
            in_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            in_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            in_type.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
            in_type.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            in_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            transform.SetInputType(in_id, &in_type, 0).context("encoder recusou o formato de entrada")?;

            let info = transform.GetOutputStreamInfo(out_id)?;
            let provides_samples = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;

            let mut encoder = Self {
                events: transform.cast()?,
                transform,
                control: EncoderControl::MediaFoundation(codec),
                _manager: manager,
                in_id,
                out_id,
                provides_samples,
                fps,
                sequence_header: Vec::new(),
                name,
            };
            encoder.refresh_sequence_header();

            encoder.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).context("BEGIN_STREAMING")?;
            encoder.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            Ok(encoder)
        }
    }

    pub fn control(&self) -> EncoderControl {
        self.control.clone()
    }

    fn refresh_sequence_header(&mut self) {
        unsafe {
            let Ok(t) = self.transform.GetOutputCurrentType(self.out_id) else { return };
            let Ok(size) = t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) else { return };
            let mut buf = vec![0u8; size as usize];
            if t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut buf, None).is_ok() {
                self.sequence_header = buf;
            }
        }
    }

    /// Laço principal (bloqueante). `next_input(n)` devolve a textura NV12 do quadro n
    /// (e é quem controla o ritmo/fps); `on_output` recebe cada quadro comprimido.
    pub fn run(
        &mut self,
        stop: &AtomicBool,
        mut next_input: impl FnMut(u64) -> Result<ID3D11Texture2D>,
        mut on_output: impl FnMut(EncodedFrame) -> Result<()>,
    ) -> Result<()> {
        let frame_100ns = 10_000_000i64 / self.fps as i64;
        let mut n: u64 = 0;
        let mut out = Vec::new();

        while !stop.load(Ordering::Relaxed) {
            let event = unsafe { self.events.GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0))? };
            let kind = unsafe { event.GetType()? };

            if kind == METransformNeedInput.0 as u32 {
                let texture = next_input(n)?;
                unsafe {
                    let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &texture, 0, false)?;
                    if let Ok(b2d) = buffer.cast::<IMF2DBuffer>() {
                        buffer.SetCurrentLength(b2d.GetContiguousLength()?)?;
                    }
                    let sample = MFCreateSample()?;
                    sample.AddBuffer(&buffer)?;
                    sample.SetSampleTime(n as i64 * frame_100ns)?;
                    sample.SetSampleDuration(frame_100ns)?;
                    self.transform.ProcessInput(self.in_id, &sample, 0)?;
                }
                n += 1;
            } else if kind == METransformHaveOutput.0 as u32 {
                let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: self.out_id,
                    pSample: ManuallyDrop::new(if self.provides_samples { None } else { Some(unsafe { MFCreateSample()? }) }),
                    dwStatus: 0,
                    pEvents: ManuallyDrop::new(None),
                }];
                let mut status = 0u32;
                let result = unsafe { self.transform.ProcessOutput(0, &mut buffers, &mut status) };
                let sample = unsafe { ManuallyDrop::take(&mut buffers[0].pSample) };
                drop(unsafe { ManuallyDrop::take(&mut buffers[0].pEvents) });

                match result {
                    Ok(()) => {}
                    Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                        // O encoder mudou o formato de saída: aceita o novo e segue.
                        unsafe {
                            let t = self.transform.GetOutputAvailableType(self.out_id, 0)?;
                            self.transform.SetOutputType(self.out_id, &t, 0)?;
                        }
                        self.refresh_sequence_header();
                        continue;
                    }
                    Err(e) => return Err(e.into()),
                }
                let sample = sample.context("encoder não devolveu sample")?;
                unsafe {
                    let keyframe = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
                    let time = sample.GetSampleTime().unwrap_or(0);
                    let buffer = sample.ConvertToContiguousBuffer()?;
                    let mut ptr = std::ptr::null_mut();
                    let mut len = 0u32;
                    buffer.Lock(&mut ptr, None, Some(&mut len))?;
                    let bytes = std::slice::from_raw_parts(ptr, len as usize);

                    out.clear();
                    // Garante SPS/PPS em todo keyframe (quem entra no meio precisa disso).
                    if keyframe && !has_nal(bytes, 7) {
                        out.extend_from_slice(&self.sequence_header);
                    }
                    out.extend_from_slice(bytes);
                    buffer.Unlock()?;
                    on_output(EncodedFrame { data: &out, keyframe, timestamp_us: time / 10 })?;
                }
            }
            // Outros eventos (drain etc.) são ignorados no protótipo.
        }
        Ok(())
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Ok(shutdown) = self.transform.cast::<IMFShutdown>() {
                let _ = shutdown.Shutdown();
            }
        }
    }
}

/// Procura uma NAL H.264 do tipo `nal_type` (7 = SPS) num fluxo Annex B.
fn has_nal(data: &[u8], nal_type: u8) -> bool {
    let mut i = 0;
    while i + 3 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if data[i + 3] & 0x1f == nal_type {
                return true;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}

