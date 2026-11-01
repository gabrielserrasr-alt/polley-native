//! Reações nas mensagens (👍 3), e links clicáveis.
//!
//! Reações ficam na tabela message_reactions (uma linha por pessoa + emoji). O banco copia o
//! canal/DM da mensagem para a reação, então dá para assinar só as reações da conversa aberta.

use super::*;
use crate::ReactionItem;

#[derive(Deserialize)]
struct Reaction {
    message_id: String,
    user_id: String,
    emoji: String,
}

/// Codifica um texto para ir na URL (emojis viram %F0%9F...).
fn pct(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

/// Pontuação que costuma vir colada no fim de um link ("olha isso: https://x.com.").
const TRAILING: [char; 9] = ['.', ',', ')', '!', '?', ';', ':', '"', '\''];

/// Texto da mensagem pronto para desenhar: igual ao que a pessoa digitou, mas com os links
/// azuis e clicáveis no meio da frase (e o "(editado)" em cinza).
///
/// Vira "markdown" só por dentro: todo símbolo digitado é escapado, então ninguém consegue
/// esconder um link atrás de outro texto nem mudar cores/fontes.
pub(super) fn rich_text(text: &str, edited: bool) -> slint::StyledText {
    let mut md = String::with_capacity(text.len() * 2);
    for (i, line) in text.lines().enumerate() {
        if i > 0 {
            md.push_str("\\\n"); // quebra de linha de verdade
        }
        for (j, word) in line.trim_start().split(' ').enumerate() {
            if j > 0 {
                md.push(' ');
            }
            md_word(&mut md, word);
        }
    }
    if edited {
        md.push_str(" <font color=\"#949ba4\">(editado)</font>");
    }
    slint::StyledText::from_markdown(&md).unwrap_or_else(|_| slint::StyledText::from_plain_text(text))
}

fn md_word(md: &mut String, word: &str) {
    if word.starts_with("https://") || word.starts_with("http://") {
        let url = word.trim_end_matches(TRAILING);
        let tail = &word[url.len()..];
        if url.len() <= 2000 && safe_link(url) && !url.contains(['(', ')', '<', '>', '[', ']', '\\', '`']) {
            md.push('[');
            escape(md, url);
            md.push_str("](");
            md.push_str(url);
            md.push(')');
            escape(md, tail);
            return;
        }
    }
    escape(md, word);
}

fn escape(md: &mut String, text: &str) {
    for c in text.chars() {
        if c.is_ascii_punctuation() {
            md.push('\\');
        }
        md.push(c);
    }
}

/// Só abre endereço web de verdade (nada de "file:", programas, aspas ou espaços escondidos).
fn safe_link(url: &str) -> bool {
    match reqwest::Url::parse(url) {
        Ok(u) => {
            matches!(u.scheme(), "http" | "https")
                && u.host_str().is_some()
                && !url.chars().any(|c| c.is_control() || c.is_whitespace() || c == '"')
        }
        Err(_) => false,
    }
}

impl App {
    /// Reações das mensagens que acabaram de carregar.
    pub(super) async fn load_reactions(&self, ids: Vec<String>) {
        if ids.is_empty() {
            return;
        }
        let query = format!("message_reactions?message_id=in.({})&select=message_id,user_id,emoji&order=created_at", ids.join(","));
        let Ok(list) = self.sb.select::<Vec<Reaction>>(&query).await else { return };
        let mut st = self.state.lock().unwrap();
        for id in &ids {
            st.chat.reactions.remove(id);
        }
        for r in list {
            st.chat.reactions.entry(r.message_id).or_default().push((r.user_id, r.emoji));
        }
    }

    /// Alguém reagiu ou tirou a reação (ao vivo).
    pub(super) async fn on_reaction_change(self: Arc<Self>, change: Value) {
        let changed = {
            let mut st = self.state.lock().unwrap();
            match change["type"].as_str() {
                Some("INSERT") => match serde_json::from_value::<Reaction>(change["record"].clone()) {
                    Ok(r) if st.messages.iter().any(|m| m.id == r.message_id) => {
                        let list = st.chat.reactions.entry(r.message_id).or_default();
                        let item = (r.user_id, r.emoji);
                        if !list.contains(&item) {
                            list.push(item);
                        }
                        true
                    }
                    _ => false,
                },
                // Apagada: só vem a chave (mensagem, pessoa, emoji) — é tudo que precisamos.
                Some("DELETE") => match serde_json::from_value::<Reaction>(change["old_record"].clone()) {
                    Ok(r) => {
                        if let Some(list) = st.chat.reactions.get_mut(&r.message_id) {
                            list.retain(|(u, e)| !(u == &r.user_id && e == &r.emoji));
                        }
                        true
                    }
                    _ => false,
                },
                _ => false,
            }
        };
        if changed {
            self.render_messages(false);
        }
    }

    /// Clicar numa reação (ou escolher no seletor): põe ou tira a minha.
    pub async fn react(self: Arc<Self>, message: String, emoji: String) {
        let Some(me) = self.sb.user_id() else { return };
        if emoji.is_empty() || emoji.len() > 32 {
            return;
        }
        let had = {
            let mut st = self.state.lock().unwrap();
            let list = st.chat.reactions.entry(message.clone()).or_default();
            let had = list.iter().any(|(u, e)| u == &me && e == &emoji);
            // Mostra na hora; se o banco recusar, desfaz.
            if had {
                list.retain(|(u, e)| !(u == &me && e == &emoji));
            } else {
                list.push((me.clone(), emoji.clone()));
            }
            had
        };
        self.render_messages(false);
        let result = if had {
            self.sb
                .delete("message_reactions", &format!("message_id=eq.{message}&user_id=eq.{me}&emoji=eq.{}", pct(&emoji)))
                .await
        } else {
            self.sb.insert::<Value>("message_reactions", json!({ "message_id": message, "emoji": emoji })).await.map(|_| ())
        };
        if let Err(e) = result {
            {
                let mut st = self.state.lock().unwrap();
                let list = st.chat.reactions.entry(message).or_default();
                if had {
                    list.push((me, emoji));
                } else {
                    list.retain(|(u, e)| !(u == &me && e == &emoji));
                }
            }
            self.render_messages(false);
            let msg = if e.to_string().contains("20 reações") {
                "Essa mensagem já tem reações demais.".to_string()
            } else {
                format!("Não deu para reagir: {e}")
            };
            self.ui(move |ui| ui.set_chat_error(msg.into()));
        }
    }

    /// Reações de uma mensagem já somadas: 👍 3 (a minha destacada), na ordem em que apareceram.
    pub(super) fn reaction_items(&self, list: Option<&Vec<(String, String)>>, me: &str) -> Vec<ReactionItem> {
        let mut out: Vec<ReactionItem> = Vec::new();
        for (user, emoji) in list.into_iter().flatten() {
            match out.iter_mut().find(|r| r.emoji.as_str() == emoji) {
                Some(r) => {
                    r.count += 1;
                    r.mine |= user == me;
                }
                None => out.push(ReactionItem { emoji: emoji.clone().into(), count: 1, mine: user == me }),
            }
        }
        out
    }

    /// Clicou num link e confirmou "Abrir site": abre no navegador padrão.
    pub fn open_link(&self, url: &str) {
        if !safe_link(url) {
            return;
        }
        use windows::core::{w, HSTRING};
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        unsafe {
            ShellExecuteW(None, w!("open"), &HSTRING::from(url), None, None, SW_SHOWNORMAL);
        }
    }
}
