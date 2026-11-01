//! Ferramenta de lançamento do Polley (roda só no PC de quem publica, nunca vai para os usuários).
//!
//!   polley-release gerar-chave
//!       Cria a chave de assinatura. A PRIVADA fica em %USERPROFILE%\.polley\update-key.pk8
//!       (fora do projeto e do git; faça um backup dela!). A PÚBLICA vai para polley\update-key.pub
//!       e é embutida no app.
//!
//!   polley-release assinar <instalador.exe> <versão> <url> <arquivo-de-saida.json> [novidades]
//!       Gera o latest.json: versão, endereço, SHA-256, tamanho e a assinatura de tudo isso.
//!
//! O app só aceita uma atualização se a assinatura bater com a chave pública dele. Então, mesmo que
//! alguém troque os arquivos no Storage, não consegue fazer os usuários instalarem outra coisa.

use anyhow::{bail, Context, Result};
use ring::signature::{Ed25519KeyPair, KeyPair};
use std::path::PathBuf;

fn private_key_path() -> PathBuf {
    let home = std::env::var("USERPROFILE").expect("USERPROFILE não definido");
    PathBuf::from(home).join(".polley").join("update-key.pk8")
}

fn public_key_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../polley/update-key.pub")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// O texto assinado. Precisa ser IGUAL ao `signed_message` do app (polley/src/app/update.rs).
fn signed_message(version: &str, url: &str, sha256: &str, size: u64, notes: &str) -> String {
    format!("polley-update-v1\n{version}\n{url}\n{sha256}\n{size}\n{notes}")
}

fn generate() -> Result<()> {
    let path = private_key_path();
    if path.exists() {
        bail!("já existe uma chave em {} — não vou sobrescrever (os apps instalados confiam nela)", path.display());
    }
    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).map_err(|_| anyhow::anyhow!("falha ao gerar a chave"))?;
    let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).map_err(|_| anyhow::anyhow!("chave inválida"))?;
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(&path, pkcs8.as_ref())?;
    std::fs::write(public_key_path(), hex(pair.public_key().as_ref()))?;
    println!("Chave privada: {}", path.display());
    println!("Chave pública: {}", public_key_path().display());
    println!("FAÇA UM BACKUP da chave privada (pendrive/nuvem pessoal). Sem ela não dá para lançar atualizações.");
    Ok(())
}

fn sign(setup: &str, version: &str, url: &str, out: &str, notes: &str) -> Result<()> {
    if !url.starts_with("https://") {
        bail!("a url precisa ser https");
    }
    let pkcs8 = std::fs::read(private_key_path()).context("chave privada não encontrada (rode: polley-release gerar-chave)")?;
    let pair = Ed25519KeyPair::from_pkcs8(&pkcs8).map_err(|_| anyhow::anyhow!("chave privada inválida"))?;
    // Confere se a chave privada é a par da pública que está sendo embutida no app.
    let public = std::fs::read_to_string(public_key_path()).context("polley\\update-key.pub não encontrado")?;
    if public.trim() != hex(pair.public_key().as_ref()) {
        bail!("a chave privada não combina com polley\\update-key.pub");
    }

    let bytes = std::fs::read(setup).with_context(|| format!("não achei {setup}"))?;
    let sha256 = hex(ring::digest::digest(&ring::digest::SHA256, &bytes).as_ref());
    let size = bytes.len() as u64;
    let signature = hex(pair.sign(signed_message(version, url, &sha256, size, notes).as_bytes()).as_ref());

    let manifest = serde_json::json!({
        "version": version,
        "url": url,
        "sha256": sha256,
        "size": size,
        "notes": notes,
        "signature": signature,
    });
    std::fs::write(out, serde_json::to_string_pretty(&manifest)?)?;
    println!("Assinado: {out}");
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["gerar-chave"] => generate(),
        ["assinar", setup, version, url, out] => sign(setup, version, url, out, ""),
        ["assinar", setup, version, url, out, notes] => sign(setup, version, url, out, notes),
        _ => bail!("uso: polley-release gerar-chave | assinar <instalador.exe> <versão> <url> <saida.json> [novidades]"),
    }
}
