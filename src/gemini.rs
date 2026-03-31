/// Gemini API streaming handler.
/// Calls Google's generateContent SSE endpoint and emits claude-compatible
/// JSON events so the frontend works without changes.

use axum::extract::ws::{Message, WebSocket};
use futures_util::StreamExt;

pub const MODEL: &str = "gemini-2.5-pro-preview";

// Gemini 2.5 Pro Preview pricing (per 1M tokens, ≤200k context)
const PRICE_INPUT: f64 = 1.25 / 1_000_000.0;
const PRICE_OUTPUT: f64 = 10.0 / 1_000_000.0;

pub struct GeminiResult {
    pub text: String,
    pub cost_usd: f64,
}

/// Stream a Gemini response to the WebSocket, emitting claude-compatible events.
/// Returns the full text and estimated cost.
pub async fn stream(
    ws: &mut WebSocket,
    api_key: &str,
    model: &str,
    history: &[(String, String)], // (role: "user"|"assistant", content)
    prompt: &str,
) -> Result<GeminiResult, String> {
    let client = reqwest::Client::new();
    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{}:streamGenerateContent?key={}&alt=sse",
        model, api_key
    );

    // Build contents array including history
    let mut contents: Vec<serde_json::Value> = history
        .iter()
        .map(|(role, content)| {
            let gemini_role = if role == "user" { "user" } else { "model" };
            serde_json::json!({"role": gemini_role, "parts": [{"text": content}]})
        })
        .collect();
    contents.push(serde_json::json!({
        "role": "user",
        "parts": [{"text": prompt}]
    }));

    let body = serde_json::json!({
        "contents": contents,
        "generationConfig": {
            "maxOutputTokens": 16384,
            "temperature": 1.0
        }
    });

    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("Gemini API error {}: {}", status, &text[..text.len().min(200)]));
    }

    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    let mut full_text = String::new();
    let mut input_tokens: u64 = 0;
    let mut output_tokens: u64 = 0;

    while let Some(chunk) = stream.next().await {
        let bytes = chunk.map_err(|e| e.to_string())?;
        buf.push_str(&String::from_utf8_lossy(&bytes));

        // Process complete SSE events (separated by \n\n)
        loop {
            let Some(end) = buf.find("\n\n") else { break };
            let event = buf[..end].to_string();
            buf = buf[end + 2..].to_string();

            let data = match event.strip_prefix("data: ") {
                Some(d) => d.trim(),
                None => continue,
            };
            if data == "[DONE]" || data.is_empty() {
                continue;
            }

            let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else { continue };

            // Extract text chunk
            if let Some(text) = v["candidates"][0]["content"]["parts"][0]["text"].as_str() {
                if !text.is_empty() {
                    full_text.push_str(text);
                    // Emit in claude stream-json compatible format
                    let ev = serde_json::json!({
                        "type": "assistant",
                        "message": {
                            "content": [{"type": "text", "text": text}]
                        }
                    });
                    if ws.send(Message::Text(ev.to_string().into())).await.is_err() {
                        return Err("WS disconnected".into());
                    }
                }
            }

            // Capture token usage from final chunk
            if let Some(meta) = v.get("usageMetadata") {
                input_tokens = meta["promptTokenCount"].as_u64().unwrap_or(0);
                output_tokens = meta["candidatesTokenCount"].as_u64().unwrap_or(0);
            }
        }
    }

    // Estimate cost (fall back to char-based estimate if token counts absent)
    let cost = if input_tokens > 0 || output_tokens > 0 {
        input_tokens as f64 * PRICE_INPUT + output_tokens as f64 * PRICE_OUTPUT
    } else {
        // ~4 chars per token rough estimate
        let est_in = (prompt.len() as f64) / 4.0;
        let est_out = (full_text.len() as f64) / 4.0;
        est_in * PRICE_INPUT + est_out * PRICE_OUTPUT
    };

    Ok(GeminiResult { text: full_text, cost_usd: cost })
}
