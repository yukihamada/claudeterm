/// NOU local machine streaming handler.
/// Proxies requests through nou-relay to the user's local NOU node.
/// NOU exposes an OpenAI-compatible API (/v1/chat/completions).

use axum::extract::ws::{Message, WebSocket};
use futures_util::StreamExt;

pub struct NouResult {
    pub text: String,
}

/// Stream a response from NOU node to the WebSocket, emitting claude-compatible events.
pub async fn stream(
    ws: &mut WebSocket,
    relay_url: &str,
    node_id: &str,
    model: &str,
    history: &[(String, String)], // (role, content)
    prompt: &str,
) -> Result<NouResult, String> {
    let client = reqwest::Client::new();
    let url = format!("{}/n/{}/v1/chat/completions", relay_url, node_id);

    // Build OpenAI-style messages
    let mut messages: Vec<serde_json::Value> = history
        .iter()
        .map(|(role, content)| serde_json::json!({"role": role, "content": content}))
        .collect();
    // Prefix /no_think for Qwen3 models to skip thinking mode (keeps responses concise)
    let is_qwen3 = model.to_lowercase().contains("qwen3");
    let user_content = if is_qwen3 && !prompt.starts_with("/no_think") {
        format!("/no_think {}", prompt)
    } else { prompt.to_string() };
    messages.push(serde_json::json!({"role": "user", "content": user_content}));

    let body = serde_json::json!({
        "model": model,
        "messages": messages,
        "stream": true,
        "max_tokens": 8192,
        "temperature": 0.7
    });

    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("NOU relay error: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("NOU error {}: {}", status, &text[..text.len().min(300)]));
    }

    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    let mut full_text = String::new();
    let mut done = false;

    'outer: while let Some(chunk) = stream.next().await {
        let bytes = chunk.map_err(|e| e.to_string())?;
        buf.push_str(&String::from_utf8_lossy(&bytes));

        while let Some(pos) = buf.find('\n') {
            let line = buf[..pos].trim().to_string();
            buf = buf[pos + 1..].to_string();

            if line == "data: [DONE]" { done = true; break 'outer; }
            if !line.starts_with("data: ") { continue; }

            let json_str = &line["data: ".len()..];
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(json_str) {
                let delta = &v["choices"][0]["delta"];
                // OpenAI SSE delta format — prefer content, fall back to reasoning_content
                // (Qwen3 thinking mode puts the real answer in reasoning_content)
                let text = delta["content"].as_str()
                    .filter(|s| !s.is_empty())
                    .or_else(|| delta["reasoning_content"].as_str().filter(|s| !s.is_empty()));
                if let Some(t) = text {
                    full_text.push_str(t);
                    let event = serde_json::json!({"type": "text", "text": t});
                    let _ = ws.send(Message::Text(event.to_string().into())).await;
                }
                if v["choices"][0]["finish_reason"].as_str() == Some("stop") {
                    done = true; break 'outer;
                }
            }
        }
    }
    let _ = done;

    Ok(NouResult { text: full_text })
}
