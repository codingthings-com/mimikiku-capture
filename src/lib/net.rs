use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::fs::{File, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::{Receiver, UnboundedSender};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::{AUTHORIZATION, HeaderValue};
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

use crate::audio::Shared;

#[derive(Debug, Clone, Deserialize)]
pub struct Segment {
    pub start: f64,
    pub end: f64,
    pub speaker: serde_json::Value,
    pub text: String,
}

impl Segment {
    pub fn speaker_label(&self) -> String {
        match &self.speaker {
            serde_json::Value::Number(n) => format!("Speaker {n}"),
            serde_json::Value::String(s) => s.clone(),
            _ => "Speaker ?".into(),
        }
    }

    /// Stable palette index for this speaker.
    pub fn speaker_index(&self) -> usize {
        match &self.speaker {
            serde_json::Value::Number(n) => n.as_u64().unwrap_or(0) as usize,
            serde_json::Value::String(s) => s.bytes().fold(0usize, |a, b| a.wrapping_mul(31).wrapping_add(b as usize)),
            _ => 0,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Conn {
    Connecting,
    Connected,
    Disconnected { reason: String, retry_in: u64 },
    /// Refused for good; not retrying.
    Stopped(String),
}

pub enum NetEvent {
    Conn(Conn),
    /// Server lifecycle state (`loading`, ...) — only sent when `events=1` is requested.
    ServerState(String),
    /// The server is saving this recording into the transcript with this id.
    Saving(String),
    Segments(Vec<Segment>),
    Error(String),
    /// The server refused us for good (bad token, missing transcript); no more retries.
    Fatal(String),
}

/// Where and how to connect.
#[derive(Debug, Clone)]
pub struct Target {
    pub url: String,
    /// Personal API token from the web app (Account → API tokens).
    pub token: Option<String>,
    /// Keep the transcript local: ask the server not to save it.
    pub save: bool,
    /// Add recordings to this existing transcript instead of starting a new one.
    pub transcript: Option<String>,
}

impl Target {
    /// The URL with our query parameters: always status events, plus save/transcript.
    pub fn full_url(&self) -> String {
        let mut url = with_param(&self.url, "events", "1");
        if !self.save {
            url = with_param(&url, "save", "0");
        }
        if let Some(id) = &self.transcript {
            url = with_param(&url, "transcript", id);
        }
        url
    }
}

fn with_param(url: &str, key: &str, value: &str) -> String {
    if url.contains(&format!("{key}=")) {
        url.to_string()
    } else if url.contains('?') {
        format!("{url}&{key}={value}")
    } else {
        format!("{url}?{key}={value}")
    }
}

/// Why a session ended: retry, or stop (the server won't accept us).
enum Ended {
    Retry(String),
    Fatal(String),
}

fn close_reason(code: CloseCode, reason: &str) -> Ended {
    match u16::from(code) {
        4401 => Ended::Fatal(format!(
            "not authorized ({reason}): pass --token or set MIMIKIKU_TOKEN (create one in the web app under Account → API tokens)"
        )),
        4404 => Ended::Fatal(format!("{reason}: check --transcript")),
        _ if reason.is_empty() => Ended::Retry("closed by server".into()),
        _ => Ended::Retry(format!("closed by server: {reason}")),
    }
}

/// Connect, stream audio, receive transcripts; reconnect forever with linear backoff.
pub async fn run(
    target: Target,
    output: Option<PathBuf>,
    mut audio_rx: Receiver<String>,
    tx: UnboundedSender<NetEvent>,
    shared: Arc<Shared>,
) {
    let mut out = match &output {
        Some(path) => match OpenOptions::new().append(true).create(true).open(path).await {
            Ok(f) => Some(f),
            Err(e) => {
                let _ = tx.send(NetEvent::Error(format!("output {}: {e}", path.display())));
                None
            }
        },
        None => None,
    };

    let url = target.full_url();
    let auth = match target.token.as_deref().map(|t| HeaderValue::from_str(&format!("Bearer {t}"))) {
        Some(Ok(value)) => Some(value),
        Some(Err(_)) => {
            let _ = tx.send(NetEvent::Fatal("the API token contains invalid characters".into()));
            return;
        }
        None => None,
    };

    let mut failures = 0u64;
    loop {
        let _ = tx.send(NetEvent::Conn(Conn::Connecting));
        let mut request = match url.as_str().into_client_request() {
            Ok(r) => r,
            Err(e) => {
                let _ = tx.send(NetEvent::Fatal(format!("bad --ws-url: {e}")));
                return;
            }
        };
        if let Some(value) = &auth {
            request.headers_mut().insert(AUTHORIZATION, value.clone());
        }
        let reason = match connect_async(request).await {
            Ok((ws, _)) => {
                failures = 0;
                // Anything queued while we were away is stale.
                while audio_rx.try_recv().is_ok() {}
                shared.connected.store(true, Ordering::Relaxed);
                let _ = tx.send(NetEvent::Conn(Conn::Connected));
                let ended = session(ws, &mut audio_rx, &tx, &mut out).await;
                shared.connected.store(false, Ordering::Relaxed);
                match ended {
                    Ended::Retry(reason) => reason,
                    Ended::Fatal(reason) => {
                        let _ = tx.send(NetEvent::Fatal(reason));
                        return;
                    }
                }
            }
            Err(e) => {
                failures += 1;
                e.to_string()
            }
        };
        let retry_in = (failures + 1).min(10);
        for left in (1..=retry_in).rev() {
            let _ = tx.send(NetEvent::Conn(Conn::Disconnected { reason: reason.clone(), retry_in: left }));
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

async fn session(
    ws: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    audio_rx: &mut Receiver<String>,
    tx: &UnboundedSender<NetEvent>,
    out: &mut Option<File>,
) -> Ended {
    let (mut write, mut read) = ws.split();
    loop {
        tokio::select! {
            msg = read.next() => match msg {
                Some(Ok(Message::Close(Some(frame)))) => return close_reason(frame.code, &frame.reason),
                None | Some(Ok(Message::Close(None))) => return Ended::Retry("closed by server".into()),
                Some(Err(e)) => return Ended::Retry(e.to_string()),
                Some(Ok(Message::Text(text))) => handle_text(&text, tx, out).await,
                Some(Ok(_)) => {}
            },
            audio = audio_rx.recv() => match audio {
                None => return Ended::Retry("shutting down".into()),
                Some(a) => if let Err(e) = write.send(Message::Text(a.into())).await {
                    return Ended::Retry(format!("send failed: {e}"));
                },
            },
        }
    }
}

async fn handle_text(text: &str, tx: &UnboundedSender<NetEvent>, out: &mut Option<File>) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else { return };
    if let Some(state) = value.get("state").and_then(|s| s.as_str()) {
        let _ = tx.send(NetEvent::ServerState(state.to_string()));
        return;
    }
    if value.get("type").and_then(|t| t.as_str()) == Some("transcript") {
        if let Some(id) = value.get("id").and_then(|i| i.as_str()) {
            let _ = tx.send(NetEvent::Saving(id.to_string()));
        }
        return;
    }
    let Ok(segments) = serde_json::from_value::<Vec<Segment>>(value) else { return };
    if segments.is_empty() {
        return;
    }
    if let Some(file) = out
        && let Err(e) = file.write_all(format!("{text}\n").as_bytes()).await
    {
        let _ = tx.send(NetEvent::Error(format!("output write: {e}")));
    }
    let _ = tx.send(NetEvent::Segments(segments));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(url: &str) -> Target {
        Target { url: url.into(), token: None, save: true, transcript: None }
    }

    #[test]
    fn full_url_adds_params_once() {
        assert_eq!(target("ws://h/listen").full_url(), "ws://h/listen?events=1");
        assert_eq!(target("ws://h/listen?events=1").full_url(), "ws://h/listen?events=1");
        let t = Target { save: false, transcript: Some("abc".into()), ..target("ws://h/listen?x=1") };
        assert_eq!(t.full_url(), "ws://h/listen?x=1&events=1&save=0&transcript=abc");
    }

    #[test]
    fn auth_and_missing_transcript_closes_are_fatal() {
        assert!(matches!(close_reason(CloseCode::from(4401), "Sign in required"), Ended::Fatal(_)));
        assert!(matches!(close_reason(CloseCode::from(4404), "Transcript not found"), Ended::Fatal(_)));
        assert!(matches!(close_reason(CloseCode::Normal, ""), Ended::Retry(_)));
    }
}
