//! Atualização automática.
//!
//! 1. Ao abrir (e a cada 6 h) baixa o `latest.json` do bucket público `releases` do Storage.
//! 2. Confere a ASSINATURA dele com a chave pública embutida no app (Ed25519). Sem assinatura
//!    válida, ignora — mesmo que alguém troque os arquivos no servidor.
//! 3. Se a versão for mais nova, baixa o instalador em segundo plano para
//!    %LOCALAPPDATA%\Polley\updates e confere o SHA-256 (que veio dentro do texto assinado).
//! 4. Mostra a faixa "Nova versão pronta". Só instala quando a pessoa clica em "Reiniciar":
//!    roda o instalador em modo silencioso (/S) e ele reabre o Polley no fim (/relancar).

use super::*;
use ring::signature::{UnparsedPublicKey, ED25519};
use std::path::PathBuf;

const PUBLIC_KEY: &str = env!("POLLEY_UPDATE_KEY");
const CURRENT: &str = env!("CARGO_PKG_VERSION");
/// Maior instalador aceito.
const MAX_SETUP: u64 = 300 * 1024 * 1024;
const MAX_MANIFEST: usize = 16 * 1024;
const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Deserialize)]
struct Manifest {
    version: String,
    url: String,
    sha256: String,
    size: u64,
    #[serde(default)]
    notes: String,
    signature: String,
}

/// Instalador baixado e conferido, esperando o clique em "Reiniciar".
pub(super) struct Ready {
    path: PathBuf,
    sha256: String,
}

/// O texto assinado. Precisa ser IGUAL ao do release-tool.
fn signed_message(m: &Manifest) -> String {
    format!("polley-update-v1\n{}\n{}\n{}\n{}\n{}", m.version, m.url, m.sha256, m.size, m.notes)
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// "1.2.3" → (1, 2, 3). Qualquer outra coisa → None (e a atualização é ignorada).
fn parse_version(v: &str) -> Option<(u32, u32, u32)> {
    let mut it = v.split('.').map(|p| p.parse::<u32>().ok());
    let r = (it.next()??, it.next()??, it.next()??);
    it.next().is_none().then_some(r)
}

/// Só instala versão MAIS NOVA: um latest.json antigo (mas assinado) não consegue fazer o app
/// voltar para uma versão velha que tinha algum problema.
fn is_newer(newer: (u32, u32, u32), current: (u32, u32, u32)) -> bool {
    newer > current
}

fn updates_dir() -> PathBuf {
    let base = std::env::var("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|_| std::env::temp_dir());
    base.join("Polley").join("updates")
}

fn http() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .https_only(true)
        .connect_timeout(Duration::from_secs(15))
        .user_agent(concat!("Polley/", env!("CARGO_PKG_VERSION")))
        .build()?)
}

/// Baixa e confere o latest.json. `Ok(None)` = já estou na versão mais nova.
async fn fetch_manifest() -> Result<Option<Manifest>> {
    // O "?t=" evita pegar uma cópia antiga guardada pelo CDN do Storage.
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs() / 600;
    let url = format!("{}/storage/v1/object/public/releases/latest.json?t={t}", crate::supabase::URL);
    let mut resp = http()?.get(url).send().await?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND || resp.status() == reqwest::StatusCode::BAD_REQUEST {
        return Ok(None); // ainda não publicamos nenhuma versão
    }
    resp.error_for_status_ref()?;
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_MANIFEST {
            anyhow::bail!("latest.json grande demais");
        }
    }
    let m: Manifest = serde_json::from_slice(&body)?;

    // Assinatura primeiro: nada do manifesto é usado antes de ela bater.
    let key = unhex(PUBLIC_KEY).ok_or_else(|| anyhow::anyhow!("chave pública inválida"))?;
    let sig = unhex(&m.signature).ok_or_else(|| anyhow::anyhow!("assinatura malformada"))?;
    UnparsedPublicKey::new(&ED25519, key)
        .verify(signed_message(&m).as_bytes(), &sig)
        .map_err(|_| anyhow::anyhow!("assinatura inválida"))?;

    let (Some(new), Some(cur)) = (parse_version(&m.version), parse_version(CURRENT)) else {
        anyhow::bail!("versão inválida");
    };
    if !is_newer(new, cur) {
        return Ok(None);
    }
    if !m.url.starts_with("https://") || m.size == 0 || m.size > MAX_SETUP || m.sha256.len() != 64 {
        anyhow::bail!("manifesto inválido");
    }
    Ok(Some(m))
}

/// SHA-256 de um arquivo já baixado (para não baixar de novo o que já está pronto).
fn file_sha256(path: &std::path::Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(hex(ring::digest::digest(&ring::digest::SHA256, &bytes).as_ref()))
}

/// Baixa o instalador para `dest`, conferindo tamanho e SHA-256.
async fn download(m: &Manifest, dest: &std::path::Path) -> Result<()> {
    use std::io::Write;
    let part = dest.with_extension("part");
    let mut resp = http()?.get(&m.url).send().await?.error_for_status()?;
    let mut file = std::fs::File::create(&part)?;
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    let mut total = 0u64;
    let result: Result<()> = async {
        while let Some(chunk) = resp.chunk().await? {
            total += chunk.len() as u64;
            if total > m.size {
                anyhow::bail!("instalador maior que o esperado");
            }
            ctx.update(&chunk);
            file.write_all(&chunk)?;
        }
        file.flush()?;
        if total != m.size || hex(ctx.finish().as_ref()) != m.sha256 {
            anyhow::bail!("o instalador baixado não confere com a assinatura");
        }
        Ok(())
    }
    .await;
    drop(file);
    match result {
        Ok(()) => {
            let _ = std::fs::remove_file(dest);
            std::fs::rename(&part, dest)?;
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            Err(e)
        }
    }
}

impl App {
    /// Roda para sempre em segundo plano: procura atualização ao abrir e a cada 6 h.
    pub async fn watch_updates(self: Arc<Self>) {
        // Deixa o app terminar de abrir antes de usar a rede para isso.
        tokio::time::sleep(Duration::from_secs(10)).await;
        loop {
            if self.update.lock().unwrap().is_none() {
                if let Err(e) = self.check_update().await {
                    eprintln!("atualização: {e:#}");
                }
            }
            tokio::time::sleep(CHECK_EVERY).await;
        }
    }

    async fn check_update(self: &Arc<Self>) -> Result<()> {
        let Some(m) = fetch_manifest().await? else { return Ok(()) };
        let dir = updates_dir();
        std::fs::create_dir_all(&dir)?;
        let dest = dir.join(format!("Polley-Setup-{}.exe", m.version));
        // Apaga instaladores velhos que ficaram na pasta.
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                if e.path() != dest {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        if file_sha256(&dest).as_deref() != Some(m.sha256.as_str()) {
            download(&m, &dest).await?;
        }
        *self.update.lock().unwrap() = Some(Ready { path: dest, sha256: m.sha256 });
        let (version, notes) = (m.version, m.notes);
        self.ui(move |ui| {
            ui.set_update_notes(notes.into());
            ui.set_update_version(version.into());
        });
        Ok(())
    }

    /// Botão "Reiniciar": fecha o Polley e roda o instalador em silêncio (ele reabre o app no fim).
    pub fn install_update(self: &Arc<Self>) {
        let Some((path, sha256)) = self.update.lock().unwrap().as_ref().map(|r| (r.path.clone(), r.sha256.clone())) else {
            return;
        };
        // Confere de novo logo antes de rodar (o arquivo pode ter sido mexido desde o download).
        if file_sha256(&path).as_deref() != Some(sha256.as_str()) {
            *self.update.lock().unwrap() = None;
            self.ui(|ui| ui.set_update_version("".into()));
            return;
        }
        self.leave_voice();
        match std::process::Command::new(&path).args(["/S", "/relancar"]).spawn() {
            Ok(_) => {
                let _ = slint::quit_event_loop();
            }
            Err(e) => {
                let msg = format!("Não consegui abrir o instalador: {e}");
                self.ui(move |ui| ui.set_update_notes(msg.into()));
            }
        }
    }
}
