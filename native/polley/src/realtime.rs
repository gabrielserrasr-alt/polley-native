//! Cliente mínimo do Supabase Realtime (protocolo Phoenix sobre WebSocket).
//! - postgres_changes: mensagens, amizades etc. chegando ao vivo;
//! - presence: quem está online e em qual canal de voz (o mesmo canal do app Electron);
//! - broadcast: avisos rápidos que não passam pelo banco (ex.: "fulano está digitando").

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::supabase::{ANON_KEY, URL};

enum Command {
    Join { topic: String },
    Leave { topic: String },
    Track { topic: String },
    Broadcast { topic: String, event: String, payload: Value },
    Token(String),
}

/// Estado da presença: chave (id do usuário) → metadados de cada conexão dele, por `phx_ref`.
pub type PresenceState = HashMap<String, Vec<Value>>;

enum Handler {
    Changes(mpsc::UnboundedSender<Value>),
    Presence {
        tx: mpsc::UnboundedSender<PresenceState>,
        state: HashMap<String, HashMap<String, Value>>,
        /// O que eu anuncio (reenviado sozinho quando a conexão cai e volta).
        track: Option<Value>,
    },
}

struct Topic {
    config: Value,
    handler: Handler,
}

type Topics = Arc<Mutex<HashMap<String, Topic>>>;

#[derive(Clone)]
pub struct Realtime {
    tx: mpsc::UnboundedSender<Command>,
    topics: Topics,
}

static REF: AtomicU64 = AtomicU64::new(1);
fn next_ref() -> String {
    REF.fetch_add(1, Ordering::Relaxed).to_string()
}

impl Realtime {
    /// Abre a conexão (e reconecta sozinho se cair).
    pub fn connect(access_token: String) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let topics: Topics = Arc::default();
        tokio::spawn(run(rx, topics.clone(), access_token));
        Self { tx, topics }
    }

    /// Assina INSERT/UPDATE/DELETE de uma tabela, com filtro opcional (ex.: `channel_id=eq.<id>`).
    /// A RLS vale aqui também: só chegam linhas que a conta pode ler.
    /// Cada mudança chega no receiver como o JSON `data` do Supabase ({type, record, old_record}).
    /// Broadcasts do mesmo tópico chegam como `{type: "BROADCAST", event, payload}`.
    pub fn subscribe(&self, topic: &str, table: &str, filter: &str) -> mpsc::UnboundedReceiver<Value> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut change = json!({ "event": "*", "schema": "public", "table": table });
        if !filter.is_empty() {
            change["filter"] = filter.into();
        }
        let config = json!({
            "broadcast": { "ack": false, "self": false },
            "presence": { "key": "" },
            "postgres_changes": [change],
            "private": false
        });
        self.add(topic, config, Handler::Changes(tx));
        rx
    }

    /// Só escuta broadcasts de um tópico. `private` = tópico protegido pelas regras do banco
    /// (tabela realtime.messages): só entra quem as regras deixarem.
    pub fn listen(&self, topic: &str, private: bool) -> mpsc::UnboundedReceiver<Value> {
        let (tx, rx) = mpsc::unbounded_channel();
        let config = json!({
            "broadcast": { "ack": false, "self": false },
            "presence": { "key": "" },
            "postgres_changes": [],
            "private": private
        });
        self.add(topic, config, Handler::Changes(tx));
        rx
    }

    /// Entra num canal de presença com a minha chave (id do usuário). O receiver recebe o estado
    /// completo (todo mundo online) a cada mudança.
    pub fn presence(&self, topic: &str, key: &str) -> mpsc::UnboundedReceiver<PresenceState> {
        let (tx, rx) = mpsc::unbounded_channel();
        let config = json!({
            "broadcast": { "ack": false, "self": false },
            "presence": { "key": key, "enabled": true },
            "postgres_changes": [],
            "private": false
        });
        self.add(topic, config, Handler::Presence { tx, state: HashMap::new(), track: None });
        rx
    }

    /// Anuncia (ou atualiza) meus dados na presença, ex.: `{ user_id, voice }`.
    pub fn track(&self, topic: &str, payload: Value) {
        let topic = format!("realtime:{topic}");
        if let Some(Topic { handler: Handler::Presence { track, .. }, .. }) = self.topics.lock().unwrap().get_mut(&topic) {
            *track = Some(payload);
        }
        let _ = self.tx.send(Command::Track { topic });
    }

    /// Manda um aviso para quem está no tópico (não passa pelo banco, não fica salvo).
    pub fn broadcast(&self, topic: &str, event: &str, payload: Value) {
        let topic = format!("realtime:{topic}");
        let _ = self.tx.send(Command::Broadcast { topic, event: event.to_string(), payload });
    }

    fn add(&self, topic: &str, config: Value, handler: Handler) {
        let topic = format!("realtime:{topic}");
        self.topics.lock().unwrap().insert(topic.clone(), Topic { config, handler });
        let _ = self.tx.send(Command::Join { topic });
    }

    pub fn unsubscribe(&self, topic: &str) {
        let topic = format!("realtime:{topic}");
        self.topics.lock().unwrap().remove(&topic);
        let _ = self.tx.send(Command::Leave { topic });
    }

    /// Token novo (a cada renovação da sessão) — o Realtime usa ele para aplicar a RLS.
    pub fn set_token(&self, token: String) {
        let _ = self.tx.send(Command::Token(token));
    }
}

fn msg(topic: &str, event: &str, payload: Value) -> Message {
    let r = next_ref();
    Message::text(json!({ "topic": topic, "event": event, "payload": payload, "ref": r, "join_ref": r }).to_string())
}

/// Mensagens para (re)entrar num tópico: join + o meu "track", se houver.
fn join_msgs(topics: &Topics, topic: &str, token: &str) -> Vec<Message> {
    let map = topics.lock().unwrap();
    let Some(t) = map.get(topic) else { return vec![] };
    let mut out = vec![msg(topic, "phx_join", json!({ "config": t.config, "access_token": token }))];
    if let Handler::Presence { track: Some(p), .. } = &t.handler {
        out.push(track_msg(topic, p));
    }
    out
}

fn track_msg(topic: &str, payload: &Value) -> Message {
    msg(topic, "presence", json!({ "type": "presence", "event": "track", "payload": payload }))
}

/// Aplica `presence_state` (estado completo) ou `presence_diff` (entradas/saídas) e avisa o App.
fn on_presence(topics: &Topics, topic: &str, event: &str, payload: &Value) {
    let mut map = topics.lock().unwrap();
    let Some(Topic { handler: Handler::Presence { tx, state, .. }, .. }) = map.get_mut(topic) else { return };
    let apply = |state: &mut HashMap<String, HashMap<String, Value>>, part: &Value, join: bool| {
        for (key, entry) in part.as_object().unwrap_or(&Map::new()) {
            let metas = entry["metas"].as_array().cloned().unwrap_or_default();
            let slot = state.entry(key.clone()).or_default();
            for meta in metas {
                let r = meta["phx_ref"].as_str().unwrap_or_default().to_string();
                if join {
                    slot.insert(r, meta);
                } else {
                    slot.remove(&r);
                }
            }
            if slot.is_empty() {
                state.remove(key);
            }
        }
    };
    if event == "presence_state" {
        state.clear();
        apply(state, payload, true);
    } else {
        apply(state, &payload["joins"], true);
        apply(state, &payload["leaves"], false);
    }
    let snapshot: PresenceState = state.iter().map(|(k, v)| (k.clone(), v.values().cloned().collect())).collect();
    let _ = tx.send(snapshot);
}

async fn run(mut rx: mpsc::UnboundedReceiver<Command>, topics: Topics, mut token: String) {
    let ws_url = format!("{}/realtime/v1/websocket?apikey={ANON_KEY}&vsn=1.0.0", URL.replacen("https://", "wss://", 1));
    let mut backoff = 1;
    loop {
        let Ok((ws, _)) = tokio_tungstenite::connect_async(ws_url.as_str()).await else {
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            backoff = (backoff * 2).min(30);
            continue;
        };
        backoff = 1;
        let (mut write, mut read) = ws.split();

        // (Re)entra em todos os tópicos assinados.
        let names: Vec<String> = topics.lock().unwrap().keys().cloned().collect();
        for topic in names {
            for m in join_msgs(&topics, &topic, &token) {
                let _ = write.send(m).await;
            }
        }

        let mut heartbeat = tokio::time::interval(Duration::from_secs(25));
        loop {
            tokio::select! {
                _ = heartbeat.tick() => {
                    let hb = json!({ "topic": "phoenix", "event": "heartbeat", "payload": {}, "ref": next_ref() });
                    if write.send(Message::text(hb.to_string())).await.is_err() { break; }
                }
                cmd = rx.recv() => {
                    let Some(cmd) = cmd else { return };
                    let out = match cmd {
                        Command::Join { topic } => join_msgs(&topics, &topic, &token),
                        Command::Leave { topic } => vec![msg(&topic, "phx_leave", json!({}))],
                        Command::Track { topic } => {
                            let map = topics.lock().unwrap();
                            match map.get(&topic) {
                                Some(Topic { handler: Handler::Presence { track: Some(p), .. }, .. }) => vec![track_msg(&topic, p)],
                                _ => vec![],
                            }
                        }
                        Command::Broadcast { topic, event, payload } => {
                            vec![msg(&topic, "broadcast", json!({ "type": "broadcast", "event": event, "payload": payload }))]
                        }
                        Command::Token(t) => {
                            token = t;
                            let names: Vec<String> = topics.lock().unwrap().keys().cloned().collect();
                            names.iter().map(|t| msg(t, "access_token", json!({ "access_token": token }))).collect()
                        }
                    };
                    let mut failed = false;
                    for m in out {
                        if write.send(m).await.is_err() { failed = true; break; }
                    }
                    if failed { break; }
                }
                incoming = read.next() => {
                    let Some(Ok(frame)) = incoming else { break };
                    let Ok(text) = frame.to_text() else { continue };
                    let Ok(v) = serde_json::from_str::<Value>(text) else { continue };
                    let topic = v["topic"].as_str().unwrap_or_default();
                    match v["event"].as_str().unwrap_or_default() {
                        "postgres_changes" => {
                            if let Some(Topic { handler: Handler::Changes(tx), .. }) = topics.lock().unwrap().get(topic) {
                                let _ = tx.send(v["payload"]["data"].clone());
                            }
                        }
                        "broadcast" => {
                            if let Some(Topic { handler: Handler::Changes(tx), .. }) = topics.lock().unwrap().get(topic) {
                                let p = &v["payload"];
                                let _ = tx.send(json!({ "type": "BROADCAST", "event": p["event"], "payload": p["payload"] }));
                            }
                        }
                        e @ ("presence_state" | "presence_diff") => on_presence(&topics, topic, e, &v["payload"]),
                        _ => {}
                    }
                }
            }
        }
        // Caiu: tenta de novo em 1 s.
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}
