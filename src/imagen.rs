/// Gemini Imagen — generate images via gemini-3-pro-image-preview
/// Returns base64-encoded PNG/JPEG as a data URL string.

const MODEL: &str = "gemini-3-pro-image-preview";

pub async fn generate(api_key: &str, prompt: &str) -> Result<(String, String), String> {
    let client = reqwest::Client::new();
    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
        MODEL, api_key
    );

    let body = serde_json::json!({
        "contents": [{"parts": [{"text": prompt}]}],
        "generationConfig": {
            "responseModalities": ["IMAGE", "TEXT"]
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
        return Err(format!("Imagen API error {}: {}", status, &text[..text.len().min(300)]));
    }

    let v: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;

    let parts = v["candidates"][0]["content"]["parts"]
        .as_array()
        .ok_or("no parts in response")?;

    for part in parts {
        if let Some(inline) = part.get("inlineData") {
            let mime = inline["mimeType"].as_str().unwrap_or("image/png").to_string();
            let data = inline["data"].as_str().ok_or("no data field")?;
            return Ok((mime, data.to_string()));
        }
    }

    Err("No image in response".into())
}
