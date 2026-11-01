// 1. Compila a interface (.slint) para Rust.
// 2. Embute as configurações do .env do projeto (URL do Supabase etc.) no executável,
//    igual ao Vite faz no app Electron.
use std::collections::HashMap;

fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent-dark".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("erro na interface .slint");

    let env_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.env");
    println!("cargo:rerun-if-changed={}", env_path.display());
    let text = std::fs::read_to_string(&env_path).expect("crie o arquivo .env na pasta Polley (veja o README)");
    let vars: HashMap<_, _> = text
        .lines()
        .filter_map(|l| l.trim().trim_start_matches('\u{feff}').split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    for (from, to) in [
        ("VITE_SUPABASE_URL", "POLLEY_SUPABASE_URL"),
        ("VITE_SUPABASE_ANON_KEY", "POLLEY_SUPABASE_ANON_KEY"),
        ("VITE_LIVEKIT_URL", "POLLEY_LIVEKIT_URL"),
    ] {
        let value = vars.get(from).cloned().unwrap_or_default();
        println!("cargo:rustc-env={to}={value}");
    }

    // Chave pública das atualizações (a privada fica só no PC de quem publica; veja release-tool).
    println!("cargo:rerun-if-changed=update-key.pub");
    let key = std::fs::read_to_string("update-key.pub").expect("falta polley/update-key.pub (rode: cargo run -p polley-release -- gerar-chave)");
    println!("cargo:rustc-env=POLLEY_UPDATE_KEY={}", key.trim());

    // 3. Ícone e informações do .exe (o que aparece no Explorer, na barra de tarefas e em "Detalhes").
    println!("cargo:rerun-if-changed=assets/polley.ico");
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/polley.ico")
        .set("ProductName", "Polley")
        .set("FileDescription", "Polley")
        .set("CompanyName", "Polley")
        .set("LegalCopyright", "Polley");
    res.compile().expect("não consegui embutir o ícone no .exe");
}
