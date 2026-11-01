// Compila a ponte em C++ para o NVENC (usa o cabeçalho oficial da NVIDIA em nvenc/).
fn main() {
    println!("cargo:rerun-if-changed=nvenc/bridge.cpp");
    println!("cargo:rerun-if-changed=nvenc/nvEncodeAPI.h");
    cc::Build::new()
        .cpp(true)
        .file("nvenc/bridge.cpp")
        .include("nvenc")
        .flag_if_supported("/utf-8")
        .warnings(false)
        .compile("polley_nvenc");
}
