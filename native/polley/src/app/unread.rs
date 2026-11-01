//! Mensagens não lidas e avisos.
//!
//! - O app já recebe ao vivo TODA mensagem das minhas conversas (assinatura "me:messages"; a
//!   RLS só entrega as minhas). Mensagem de outra pessoa numa conversa que não está aberta
//!   vira +1 na bolinha da DM / do canal / do servidor.
//! - DM ou menção (@meu_usuario): "plim" e, se o Polley não estiver em foco, o ícone pisca na
//!   barra de tarefas.
//! - O que chega com o app FECHADO: guardamos quando ele esteve aberto pela última vez
//!   (`last_seen`) e, ao abrir, buscamos de uma vez o que chegou desde então.
//!
//! Fica em %APPDATA%\Polley\read.json (ou read-<perfil>.json).

use super::*;
use chrono::Utc;
use serde::Serialize;
use std::path::PathBuf;

/// Máximo de mensagens buscadas ao abrir o app (o resto não entra na contagem).
const CATCH_UP_LIMIT: usize = 500;

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct ReadFile {
    /// Última vez que o app estava aberto e recebendo mensagens.
    last_seen: Option<String>,
    /// Conversa → até quando eu já li.
    last_read: HashMap<String, String>,
    /// Não lidas que ficaram quando o app fechou (conversa → quantas / se tinha menção).
    pending: HashMap<String, u32>,
    pending_mentions: HashSet<String>,
}

fn file() -> PathBuf {
    let base = std::env::var("APPDATA").map(PathBuf::from).unwrap_or_else(|_| std::env::temp_dir());
    let mut args = std::env::args().skip_while(|a| a != "--perfil").skip(1);
    let name = match args.next() {
        Some(p) => format!("read-{}.json", p.replace(['/', '\\', '.'], "")),
        None => "read.json".into(),
    };
    base.join("Polley").join(name)
}

#[derive(Default)]
pub(super) struct Unread {
    file: ReadFile,
    /// Conversa (canal ou DM) → não lidas.
    counts: HashMap<String, u32>,
    /// Conversas com menção a mim não lida.
    mentions: HashSet<String>,
    /// Canal → servidor dele (para acender o servidor).
    channel_server: HashMap<String, String>,
}

impl Unread {
    fn save(&mut self) {
        self.file.last_seen = Some(Utc::now().to_rfc3339());
        self.file.pending = self.counts.clone();
        self.file.pending_mentions = self.mentions.clone();
        let path = file();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_vec(&self.file) {
            let _ = std::fs::write(path, json);
        }
    }

    /// Canal conhecido (vindo da lista de canais de um servidor).
    pub fn note_channel(&mut self, channel: &str, server: &str) {
        self.channel_server.insert(channel.to_string(), server.to_string());
    }

    pub fn count(&self, chat: &str) -> u32 {
        self.counts.get(chat).copied().unwrap_or(0)
    }

    /// Algum canal deste servidor tem não lida? E quantas menções?
    pub fn server(&self, server: &str) -> (bool, u32) {
        let mut unread = false;
        let mut mentions = 0;
        for (chat, n) in &self.counts {
            if self.channel_server.get(chat).map(String::as_str) == Some(server) && *n > 0 {
                unread = true;
                if self.mentions.contains(chat) {
                    mentions += n;
                }
            }
        }
        (unread, mentions)
    }
}

fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s).ok().map(|t| t.with_timezone(&Utc))
}

// ------------------------------------------------------------ janela do Windows

mod window {
    use windows::core::BOOL;
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::Win32::System::Threading::GetCurrentProcessId;
    use windows::Win32::UI::WindowsAndMessaging::*;

    fn pid_of(w: HWND) -> u32 {
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(w, Some(&mut pid)) };
        pid
    }

    /// O Polley é a janela em foco?
    pub fn focused() -> bool {
        let w = unsafe { GetForegroundWindow() };
        !w.is_invalid() && pid_of(w) == unsafe { GetCurrentProcessId() }
    }

    /// Faz o ícone do Polley piscar na barra de tarefas até a pessoa abrir a janela.
    pub fn flash() {
        unsafe extern "system" fn each(w: HWND, data: LPARAM) -> BOOL {
            let found = &mut *(data.0 as *mut Option<HWND>);
            let owner = GetWindow(w, GW_OWNER).unwrap_or_default();
            if IsWindowVisible(w).as_bool() && owner.is_invalid() && pid_of(w) == GetCurrentProcessId() {
                *found = Some(w);
                return false.into();
            }
            true.into()
        }
        let mut found: Option<HWND> = None;
        unsafe {
            let _ = EnumWindows(Some(each), LPARAM(&mut found as *mut _ as isize));
            if let Some(w) = found {
                let info = FLASHWINFO {
                    cbSize: std::mem::size_of::<FLASHWINFO>() as u32,
                    hwnd: w,
                    dwFlags: FLASHW_TRAY | FLASHW_TIMERNOFG,
                    uCount: 0,
                    dwTimeout: 0,
                };
                let _ = FlashWindowEx(&info);
            }
        }
    }
}

impl App {
    fn my_username(&self) -> String {
        let me = self.sb.user_id().unwrap_or_default();
        self.state.lock().unwrap().profiles.get(&me).map(|p| p.username.to_lowercase()).unwrap_or_default()
    }

    fn mentions_me(&self, content: &str, username: &str) -> bool {
        if username.is_empty() {
            return false;
        }
        let content = content.to_lowercase();
        let tag = format!("@{username}");
        // "@joao" conta, "@joaozinho" não.
        content.match_indices(&tag).any(|(i, _)| {
            content[i + tag.len()..].chars().next().is_none_or(|c| !(c.is_alphanumeric() || c == '_'))
        })
    }

    /// Depois do login: canais → servidor, não lidas que ficaram de antes e o que chegou com o app fechado.
    pub(super) async fn start_unread(self: &Arc<Self>) {
        let me = self.sb.user_id().unwrap_or_default();
        let channels: Vec<Value> = self.sb.select("channels?select=id,server_id").await.unwrap_or_default();
        let saved: ReadFile = std::fs::read(file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let since = saved.last_seen.clone();
        {
            let mut st = self.state.lock().unwrap();
            let u = &mut st.unread;
            for c in &channels {
                if let (Some(id), Some(server)) = (c["id"].as_str(), c["server_id"].as_str()) {
                    u.channel_server.insert(id.into(), server.into());
                }
            }
            u.counts = saved.pending.clone();
            u.mentions = saved.pending_mentions.clone();
            u.file = saved;
        }
        if let Some(since) = since {
            let username = self.my_username();
            let query = format!(
                "messages?select=channel_id,dm_id,created_at,content&author_id=neq.{me}&created_at=gt.{}&order=created_at&limit={CATCH_UP_LIMIT}",
                since.replace('+', "%2B")
            );
            let missed: Vec<Value> = self.sb.select(&query).await.unwrap_or_default();
            let mut st = self.state.lock().unwrap();
            for m in missed {
                let Some(chat) = m["channel_id"].as_str().or(m["dm_id"].as_str()).map(str::to_string) else { continue };
                let at = m["created_at"].as_str().and_then(parse_time);
                let read = st.unread.file.last_read.get(&chat).and_then(|t| parse_time(t));
                if matches!((at, read), (Some(a), Some(r)) if a <= r) {
                    continue;
                }
                *st.unread.counts.entry(chat.clone()).or_default() += 1;
                if self.mentions_me(m["content"].as_str().unwrap_or_default(), &username) {
                    st.unread.mentions.insert(chat);
                }
            }
        }
        self.state.lock().unwrap().unread.save();
        self.render_unread();

        // De minuto em minuto anota que o app continua aberto (se fechar à força, perde no máximo 1 min).
        let app = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                let Some(app) = app.upgrade() else { break };
                app.state.lock().unwrap().unread.save();
            }
        });
    }

    /// Chegou mensagem numa conversa minha (qualquer uma).
    pub(super) async fn on_incoming(&self, record: &Value) {
        let me = self.sb.user_id().unwrap_or_default();
        if record["author_id"].as_str() == Some(me.as_str()) {
            return;
        }
        let dm = record["dm_id"].as_str().map(str::to_string);
        let channel = record["channel_id"].as_str().map(str::to_string);
        let Some(chat) = dm.clone().or(channel.clone()) else { return };
        let mention = self.mentions_me(record["content"].as_str().unwrap_or_default(), &self.my_username());

        // Canal novo (criado depois que abri o app): descobre de qual servidor é.
        if let Some(ch) = &channel {
            let known = self.state.lock().unwrap().unread.channel_server.contains_key(ch);
            if !known {
                let rows: Vec<Value> = self.sb.select(&format!("channels?id=eq.{ch}&select=server_id")).await.unwrap_or_default();
                if let Some(server) = rows.first().and_then(|r| r["server_id"].as_str()) {
                    self.state.lock().unwrap().unread.channel_server.insert(ch.clone(), server.into());
                }
            }
        }

        let focused = window::focused();
        let open = self.state.lock().unwrap().current_channel.as_deref() == Some(&chat);
        if open {
            self.mark_read(&chat);
        } else {
            let mut st = self.state.lock().unwrap();
            *st.unread.counts.entry(chat.clone()).or_default() += 1;
            if mention {
                st.unread.mentions.insert(chat.clone());
            }
            st.unread.save();
        }
        // Avisa: DM ou menção, a não ser que eu já esteja olhando essa conversa.
        if (dm.is_some() || mention) && !(open && focused) {
            let s = crate::settings::get();
            if s.notif_sound {
                crate::sounds::notify();
            }
            if !focused && s.notif_flash {
                window::flash();
            }
            if !focused && s.notif_popup {
                let author = record["author_id"].as_str().unwrap_or_default().to_string();
                self.ensure_profiles(std::slice::from_ref(&author)).await;
                let name = self.state.lock().unwrap().profiles.get(&author).map(|p| p.name()).unwrap_or_else(|| "Polley".into());
                let title = if dm.is_some() { name } else { format!("{name} mencionou você") };
                let text: String = record["content"].as_str().unwrap_or_default().chars().take(200).collect();
                crate::tray::popup(&title, &text);
            }
        }
        if !open {
            self.render_unread();
        }
    }

    /// Abri a conversa: zera as não lidas dela.
    pub(super) fn mark_read(&self, chat: &str) {
        let had = {
            let mut st = self.state.lock().unwrap();
            let had = st.unread.counts.remove(chat).is_some();
            st.unread.mentions.remove(chat);
            st.unread.file.last_read.insert(chat.to_string(), Utc::now().to_rfc3339());
            st.unread.save();
            had
        };
        if had {
            self.render_unread();
        }
    }

    /// Redesenha bolinhas: DMs, servidores, canais e o botão de início.
    pub(super) fn render_unread(&self) {
        self.render_dms();
        self.render_servers();
        self.render_text_channels();
        let dm_total: u32 = {
            let st = self.state.lock().unwrap();
            st.dms.iter().map(|d| st.unread.count(&d.id)).sum()
        };
        // Bolinha no ícone da barra de tarefas: alguma conversa (DM, canal) com não lidas.
        let any = self.state.lock().unwrap().unread.counts.values().any(|&n| n > 0);
        let badge = any && crate::settings::get().notif_badge;
        self.ui(move |ui| {
            ui.set_home_unread(dm_total as i32);
            crate::tray::set_badge(badge);
        });
    }
}
