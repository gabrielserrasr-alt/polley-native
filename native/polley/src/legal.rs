//! Termos de Uso e Política de Privacidade: os textos vão embutidos no .exe (legal/*.txt)
//! e aparecem numa janela de leitura, no cadastro e em Configurações → Minha conta.

use crate::{AppWindow, LegalBlock};
use slint::{ModelRc, VecModel};

/// Versão aceita no cadastro (fica salva na conta). Mude quando os textos mudarem.
pub const VERSION: &str = "2026-09-27";
const VERSION_TEXT: &str = "27 de setembro de 2026";
/// Email para pedidos da LGPD e denúncias.
pub const CONTACT: &str = "suporte.polley@gmail.com";

const TERMS: &str = include_str!("../legal/termos.txt");
const PRIVACY: &str = include_str!("../legal/privacidade.txt");

/// Blocos para a interface: 0 = parágrafo, 1 = título de seção, 2 = item de lista, 3 = espaço.
fn blocks(text: &str) -> (String, Vec<LegalBlock>) {
    let text = text.replace("{versao}", VERSION_TEXT).replace("{contato}", CONTACT);
    let mut title = String::new();
    let mut out = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let (kind, text) = if let Some(h) = line.strip_prefix("# ") {
            if title.is_empty() {
                title = h.to_string();
                continue;
            }
            out.push(LegalBlock { kind: 3, text: "".into() });
            (1, h)
        } else if let Some(item) = line.strip_prefix("- ") {
            (2, item)
        } else {
            (0, line)
        };
        out.push(LegalBlock { kind, text: text.into() });
    }
    (title, out)
}

/// Abre a janela de leitura: `which` = "termos" ou "privacidade".
pub fn show(ui: &AppWindow, which: &str) {
    let (title, list) = blocks(if which == "privacidade" { PRIVACY } else { TERMS });
    ui.set_legal_title(title.into());
    ui.set_legal_blocks(ModelRc::new(VecModel::from(list)));
    ui.set_legal_open(true);
}
