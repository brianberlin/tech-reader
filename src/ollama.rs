//! Minimal async streaming client for a LOCAL Ollama server. Ported from the TS
//! `ollama/client.ts`: stream POST /api/chat (NDJSON), classify failures into a
//! discriminated [`OllamaError`] so the narrator can fall back cleanly.

use std::fmt;
use std::time::Duration;

use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::json;

/// Configuration for a chat completion against an Ollama server.
#[derive(Clone)]
pub struct OllamaConfig {
    pub base_url: String,
    pub model: String,
    pub temperature: f32,
    pub keep_alive: String,
    /// Abort a stream that produces no bytes for this long.
    pub idle_timeout: Duration,
}

impl OllamaConfig {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            temperature: 0.3,
            keep_alive: "5m".to_string(),
            idle_timeout: Duration::from_secs(60),
        }
    }
}

/// Discriminates the category of a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OllamaErrorCode {
    Unreachable,
    ModelMissing,
    Http,
    Aborted,
    Parse,
}

/// Normalized error type for every failure path.
#[derive(Debug, Clone)]
pub struct OllamaError {
    pub code: OllamaErrorCode,
    pub message: String,
}

impl OllamaError {
    fn new(code: OllamaErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
    pub fn unreachable(m: impl Into<String>) -> Self {
        Self::new(OllamaErrorCode::Unreachable, m)
    }
    pub fn parse(m: impl Into<String>) -> Self {
        Self::new(OllamaErrorCode::Parse, m)
    }
}

impl fmt::Display for OllamaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}
impl std::error::Error for OllamaError {}

fn normalize_base(url: &str) -> &str {
    url.trim_end_matches('/')
}

/// `not found` / `try pulling` indicate a missing model; everything else http.
fn from_error_text(text: &str) -> OllamaError {
    let lower = text.to_lowercase();
    if lower.contains("not found") || lower.contains("try pulling") {
        OllamaError::new(OllamaErrorCode::ModelMissing, text)
    } else {
        OllamaError::new(OllamaErrorCode::Http, text)
    }
}

/// True if the server answers GET /api/tags within `timeout`. Never errors.
pub async fn is_available(base_url: &str, timeout: Duration) -> bool {
    let base = normalize_base(base_url);
    let client = match reqwest::Client::builder().timeout(timeout).build() {
        Ok(c) => c,
        Err(_) => return false,
    };
    match client.get(format!("{base}/api/tags")).send().await {
        Ok(r) => r.status().is_success(),
        Err(_) => false,
    }
}

#[derive(Deserialize)]
struct ChatLine {
    #[serde(default)]
    message: Option<ChatMessage>,
    #[serde(default)]
    done: Option<bool>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
struct ChatMessage {
    #[serde(default)]
    content: Option<String>,
}

/// Stream a chat completion from POST /api/chat with `stream: true`, invoking
/// `on_token(delta)` for each non-empty text delta. Resolves when the terminal
/// `{"done": true}` line is read or the server closes the stream. Aborts if no
/// bytes arrive for `cfg.idle_timeout`.
pub async fn stream_chat<F>(
    cfg: &OllamaConfig,
    system: &str,
    prompt: &str,
    mut on_token: F,
) -> Result<(), OllamaError>
where
    F: FnMut(&str),
{
    let base = normalize_base(&cfg.base_url);
    let body = json!({
        "model": cfg.model,
        "stream": true,
        "options": { "temperature": cfg.temperature },
        "keep_alive": cfg.keep_alive,
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": prompt },
        ],
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/api/chat"))
        .json(&body)
        .send()
        .await
        .map_err(|e| OllamaError::unreachable(format!("Could not reach Ollama at {base}: {e}")))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let err = extract_error_text(&text).unwrap_or_else(|| format!("HTTP {status}"));
        return Err(from_error_text(&err));
    }

    let mut stream = resp.bytes_stream();
    let mut buffer = String::new();
    let mut saw_valid = false;
    let mut saw_parse_fail = false;

    loop {
        let next = tokio::time::timeout(cfg.idle_timeout, stream.next()).await;
        let chunk = match next {
            Err(_) => {
                return Err(OllamaError::unreachable(format!(
                    "Ollama produced no output for {}ms.",
                    cfg.idle_timeout.as_millis()
                )))
            }
            Ok(None) => break, // server closed the stream
            Ok(Some(Ok(b))) => b,
            Ok(Some(Err(e))) => {
                return Err(OllamaError::unreachable(format!("Lost connection to Ollama: {e}")))
            }
        };

        buffer.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(nl) = buffer.find('\n') {
            let line = buffer[..nl].trim().to_string();
            buffer.drain(..=nl);
            if line.is_empty() {
                continue;
            }
            match handle_line(&line, &mut on_token)? {
                LineResult::Done => return Ok(()),
                LineResult::Parsed => saw_valid = true,
                LineResult::ParseFail => saw_parse_fail = true,
            }
        }
    }

    // Flush a trailing line with no newline.
    let tail = buffer.trim().to_string();
    if !tail.is_empty() {
        match handle_line(&tail, &mut on_token)? {
            LineResult::Done => return Ok(()),
            LineResult::Parsed => saw_valid = true,
            LineResult::ParseFail => saw_parse_fail = true,
        }
    }

    if !saw_valid && saw_parse_fail {
        return Err(OllamaError::parse(
            "Ollama stream contained no parseable JSON lines.",
        ));
    }
    Ok(())
}

enum LineResult {
    Done,
    Parsed,
    ParseFail,
}

fn handle_line<F: FnMut(&str)>(line: &str, on_token: &mut F) -> Result<LineResult, OllamaError> {
    let obj: ChatLine = match serde_json::from_str(line) {
        Ok(o) => o,
        Err(_) => return Ok(LineResult::ParseFail),
    };
    if let Some(err) = obj.error.as_deref() {
        if !err.is_empty() {
            return Err(from_error_text(err));
        }
    }
    if let Some(content) = obj.message.and_then(|m| m.content) {
        if !content.is_empty() {
            on_token(&content);
        }
    }
    if obj.done == Some(true) {
        return Ok(LineResult::Done);
    }
    Ok(LineResult::Parsed)
}

/// Pull a human-readable error string out of an Ollama error response body.
fn extract_error_text(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if let Some(e) = v.get("error").and_then(|e| e.as_str()) {
            if !e.is_empty() {
                return Some(e.to_string());
            }
        }
    }
    Some(trimmed.to_string())
}
