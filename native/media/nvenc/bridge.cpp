// Ponte mínima para o NVENC (encoder de vídeo das placas NVIDIA), usada pelo Rust (src/nvenc.rs).
//
// Por que em C++: o cabeçalho oficial da NVIDIA (nvEncodeAPI.h, licença MIT) tem estruturas enormes
// com uniões e campos de bits. Compilando com ele, o layout fica certo por construção.
//
// Por que existe: em notebooks com 2 placas, o encoder da NVIDIA pelo caminho do Windows
// (Media Foundation) recusa abrir ("Falha catastrófica"). Falando direto com o driver — como o OBS
// faz — ele funciona e o vídeo fica todo na memória da placa (VRAM), não na RAM.
//
// A DLL do NVENC vem com o driver da NVIDIA; é carregada só na hora (PC sem NVIDIA não quebra).

#include <windows.h>
#include <d3d11.h>
#include <stdio.h>
#include <string.h>
#include <vector>
#include "nvEncodeAPI.h"

namespace {

struct Registered {
    ID3D11Texture2D* texture;
    NV_ENC_REGISTERED_PTR handle;
};

}  // namespace

struct NvBridge {
    HMODULE dll = nullptr;
    NV_ENCODE_API_FUNCTION_LIST api = {};
    void* encoder = nullptr;
    NV_ENC_OUTPUT_PTR bitstream = nullptr;
    NV_ENC_INITIALIZE_PARAMS init = {};
    NV_ENC_CONFIG config = {};
    std::vector<Registered> registered;
    uint32_t width = 0, height = 0, fps = 0;
    // Quadro entregue ao Rust e ainda não devolvido (nvb_release).
    NV_ENC_INPUT_PTR mapped = nullptr;
    bool locked = false;
};

static void set_error(char* err, int len, const char* what, NVENCSTATUS status) {
    if (err && len > 0) snprintf(err, len, "%s (código NVENC %d)", what, (int)status);
}

static void set_vbv(NV_ENC_CONFIG& config, uint32_t bitrate, uint32_t fps) {
    config.rcParams.averageBitRate = bitrate;
    config.rcParams.maxBitRate = bitrate;
    // Buffer de ~1/4 s: keyframes ainda ficam bonitos, sem acumular atraso.
    config.rcParams.vbvBufferSize = bitrate / 4;
    config.rcParams.vbvInitialDelay = bitrate / 4;
    (void)fps;
}

extern "C" void nvb_close(NvBridge* b) {
    if (!b) return;
    if (b->encoder) {
        if (b->locked) b->api.nvEncUnlockBitstream(b->encoder, b->bitstream);
        if (b->mapped) b->api.nvEncUnmapInputResource(b->encoder, b->mapped);
        for (auto& r : b->registered) {
            b->api.nvEncUnregisterResource(b->encoder, r.handle);
            r.texture->Release();
        }
        if (b->bitstream) b->api.nvEncDestroyBitstreamBuffer(b->encoder, b->bitstream);
        b->api.nvEncDestroyEncoder(b->encoder);
    }
    if (b->dll) FreeLibrary(b->dll);
    delete b;
}

// Abre o encoder H.264 na placa do `device` (ID3D11Device*). 0 = ok.
extern "C" int nvb_open(void* device, uint32_t width, uint32_t height, uint32_t fps, uint32_t bitrate,
                        NvBridge** out, char* err, int err_len) {
    *out = nullptr;
    NvBridge* b = new NvBridge();
    b->width = width;
    b->height = height;
    b->fps = fps;

    b->dll = LoadLibraryW(L"nvEncodeAPI64.dll");
    if (!b->dll) {
        if (err) snprintf(err, err_len, "driver da NVIDIA sem NVENC (nvEncodeAPI64.dll não encontrada)");
        nvb_close(b);
        return -1;
    }
    typedef NVENCSTATUS(NVENCAPI * MaxVersionFn)(uint32_t*);
    typedef NVENCSTATUS(NVENCAPI * CreateFn)(NV_ENCODE_API_FUNCTION_LIST*);
    auto max_version = (MaxVersionFn)GetProcAddress(b->dll, "NvEncodeAPIGetMaxSupportedVersion");
    auto create = (CreateFn)GetProcAddress(b->dll, "NvEncodeAPICreateInstance");
    uint32_t version = 0;
    if (!max_version || !create || max_version(&version) != NV_ENC_SUCCESS) {
        if (err) snprintf(err, err_len, "DLL do NVENC inválida");
        nvb_close(b);
        return -1;
    }
    if (version < ((NVENCAPI_MAJOR_VERSION << 4) | NVENCAPI_MINOR_VERSION)) {
        if (err) snprintf(err, err_len, "driver da NVIDIA antigo demais para o NVENC (atualize o driver)");
        nvb_close(b);
        return -1;
    }
    b->api.version = NV_ENCODE_API_FUNCTION_LIST_VER;
    NVENCSTATUS s = create(&b->api);
    if (s != NV_ENC_SUCCESS) {
        set_error(err, err_len, "NvEncodeAPICreateInstance falhou", s);
        nvb_close(b);
        return (int)s;
    }

    NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS session = {};
    session.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
    session.deviceType = NV_ENC_DEVICE_TYPE_DIRECTX;
    session.device = device;
    session.apiVersion = NVENCAPI_VERSION;
    s = b->api.nvEncOpenEncodeSessionEx(&session, &b->encoder);
    if (s != NV_ENC_SUCCESS) {
        b->encoder = nullptr;
        set_error(err, err_len, "não consegui abrir uma sessão do NVENC", s);
        nvb_close(b);
        return (int)s;
    }

    // Preset: P4 é o equilíbrio padrão; no 4K usa P2 (mais rápido) para garantir 60 fps.
    GUID preset = height > 1440 ? NV_ENC_PRESET_P2_GUID : NV_ENC_PRESET_P4_GUID;
    NV_ENC_TUNING_INFO tuning = NV_ENC_TUNING_INFO_LOW_LATENCY;
    NV_ENC_PRESET_CONFIG preset_config = {};
    preset_config.version = NV_ENC_PRESET_CONFIG_VER;
    preset_config.presetCfg.version = NV_ENC_CONFIG_VER;
    s = b->api.nvEncGetEncodePresetConfigEx(b->encoder, NV_ENC_CODEC_H264_GUID, preset, tuning, &preset_config);
    if (s != NV_ENC_SUCCESS) {
        set_error(err, err_len, "NVENC sem suporte a H.264", s);
        nvb_close(b);
        return (int)s;
    }

    b->config = preset_config.presetCfg;
    b->config.version = NV_ENC_CONFIG_VER;
    // Main: tocado por qualquer decoder (inclusive o do Electron/Chromium).
    b->config.profileGUID = NV_ENC_H264_PROFILE_MAIN_GUID;
    b->config.gopLength = fps * 2;  // keyframe a cada 2 s
    b->config.frameIntervalP = 1;   // sem B-frames = menos atraso
    b->config.rcParams.rateControlMode = NV_ENC_PARAMS_RC_CBR;
    set_vbv(b->config, bitrate, fps);
    NV_ENC_CONFIG_H264& h264 = b->config.encodeCodecConfig.h264Config;
    h264.idrPeriod = fps * 2;
    h264.repeatSPSPPS = 1;  // SPS/PPS em todo keyframe: quem entra no meio da live consegue decodificar
    h264.maxNumRefFrames = 1;

    b->init.version = NV_ENC_INITIALIZE_PARAMS_VER;
    b->init.encodeGUID = NV_ENC_CODEC_H264_GUID;
    b->init.presetGUID = preset;
    b->init.encodeWidth = width;
    b->init.encodeHeight = height;
    b->init.darWidth = width;
    b->init.darHeight = height;
    b->init.maxEncodeWidth = width;
    b->init.maxEncodeHeight = height;
    b->init.frameRateNum = fps;
    b->init.frameRateDen = 1;
    b->init.enablePTD = 1;          // o NVENC decide o tipo de cada quadro
    b->init.enableEncodeAsync = 0;  // síncrono: mais simples, e com baixa latência sai 1 quadro por entrada
    b->init.encodeConfig = &b->config;
    b->init.tuningInfo = tuning;
    s = b->api.nvEncInitializeEncoder(b->encoder, &b->init);
    if (s != NV_ENC_SUCCESS) {
        set_error(err, err_len, "o NVENC recusou a configuração (resolução alta demais?)", s);
        nvb_close(b);
        return (int)s;
    }

    NV_ENC_CREATE_BITSTREAM_BUFFER buffer = {};
    buffer.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
    s = b->api.nvEncCreateBitstreamBuffer(b->encoder, &buffer);
    if (s != NV_ENC_SUCCESS) {
        set_error(err, err_len, "NVENC sem memória para a saída", s);
        nvb_close(b);
        return (int)s;
    }
    b->bitstream = buffer.bitstreamBuffer;
    *out = b;
    return 0;
}

// Registra a textura na primeira vez que ela aparece (o conversor usa sempre as mesmas 3).
static NV_ENC_REGISTERED_PTR registered_for(NvBridge* b, ID3D11Texture2D* texture, NVENCSTATUS* status) {
    for (auto& r : b->registered)
        if (r.texture == texture) return r.handle;
    NV_ENC_REGISTER_RESOURCE reg = {};
    reg.version = NV_ENC_REGISTER_RESOURCE_VER;
    reg.resourceType = NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
    reg.width = b->width;
    reg.height = b->height;
    reg.pitch = 0;
    reg.subResourceIndex = 0;
    reg.resourceToRegister = texture;
    reg.bufferFormat = NV_ENC_BUFFER_FORMAT_NV12;
    reg.bufferUsage = NV_ENC_INPUT_IMAGE;
    *status = b->api.nvEncRegisterResource(b->encoder, &reg);
    if (*status != NV_ENC_SUCCESS) return nullptr;
    texture->AddRef();  // garante que a textura vive enquanto estiver registrada
    b->registered.push_back({texture, reg.registeredResource});
    return reg.registeredResource;
}

// Codifica a textura NV12 (ID3D11Texture2D*). Os bytes ficam válidos até nvb_release().
// `*len == 0` = o encoder ainda não entregou nada para este quadro.
extern "C" int nvb_encode(NvBridge* b, void* texture, uint64_t timestamp, int force_idr,
                          const uint8_t** data, uint32_t* len, int* keyframe) {
    *data = nullptr;
    *len = 0;
    *keyframe = 0;
    NVENCSTATUS s = NV_ENC_SUCCESS;
    NV_ENC_REGISTERED_PTR handle = registered_for(b, (ID3D11Texture2D*)texture, &s);
    if (!handle) return (int)s;

    NV_ENC_MAP_INPUT_RESOURCE map = {};
    map.version = NV_ENC_MAP_INPUT_RESOURCE_VER;
    map.registeredResource = handle;
    s = b->api.nvEncMapInputResource(b->encoder, &map);
    if (s != NV_ENC_SUCCESS) return (int)s;
    b->mapped = map.mappedResource;

    NV_ENC_PIC_PARAMS pic = {};
    pic.version = NV_ENC_PIC_PARAMS_VER;
    pic.inputWidth = b->width;
    pic.inputHeight = b->height;
    pic.inputBuffer = map.mappedResource;
    pic.bufferFmt = map.mappedBufferFmt;
    pic.outputBitstream = b->bitstream;
    pic.pictureStruct = NV_ENC_PIC_STRUCT_FRAME;
    pic.inputTimeStamp = timestamp;
    if (force_idr) pic.encodePicFlags = NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS;
    s = b->api.nvEncEncodePicture(b->encoder, &pic);
    if (s == NV_ENC_ERR_NEED_MORE_INPUT) return 0;
    if (s != NV_ENC_SUCCESS) return (int)s;

    NV_ENC_LOCK_BITSTREAM lock = {};
    lock.version = NV_ENC_LOCK_BITSTREAM_VER;
    lock.outputBitstream = b->bitstream;
    s = b->api.nvEncLockBitstream(b->encoder, &lock);  // espera o quadro ficar pronto
    if (s != NV_ENC_SUCCESS) return (int)s;
    b->locked = true;
    *data = (const uint8_t*)lock.bitstreamBufferPtr;
    *len = lock.bitstreamSizeInBytes;
    *keyframe = lock.pictureType == NV_ENC_PIC_TYPE_IDR || lock.pictureType == NV_ENC_PIC_TYPE_I;
    return 0;
}

// Devolve o quadro entregue por nvb_encode (libera a saída e a textura de entrada).
extern "C" void nvb_release(NvBridge* b) {
    if (b->locked) {
        b->api.nvEncUnlockBitstream(b->encoder, b->bitstream);
        b->locked = false;
    }
    if (b->mapped) {
        b->api.nvEncUnmapInputResource(b->encoder, b->mapped);
        b->mapped = nullptr;
    }
}

// Muda o bitrate na hora (o LiveKit pede quando a rede de quem assiste aperta).
extern "C" int nvb_set_bitrate(NvBridge* b, uint32_t bitrate) {
    set_vbv(b->config, bitrate, b->fps);
    NV_ENC_RECONFIGURE_PARAMS params = {};
    params.version = NV_ENC_RECONFIGURE_PARAMS_VER;
    params.reInitEncodeParams = b->init;
    params.reInitEncodeParams.encodeConfig = &b->config;
    return (int)b->api.nvEncReconfigureEncoder(b->encoder, &params);
}
