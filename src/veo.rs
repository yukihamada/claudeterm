/// Veo 3 + Nano Banana video/image generation via Gemini API
/// - Nano Banana: fast image generation (character sheets, storyboards)
/// - Veo 3: 8-second cinematic video clips with audio/dialogue

use serde::{Deserialize, Serialize};

const VEO_MODEL: &str = "veo-3.0-generate-001";
const NANOBANANA_MODEL: &str = "nano-banana-pro-preview";

#[derive(Serialize)]
struct VeoRequest {
    model: String,
    prompt: String,
    #[serde(rename = "generationConfig")]
    config: VeoConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    image: Option<VeoImage>,
}

#[derive(Serialize)]
struct VeoConfig {
    #[serde(rename = "aspectRatio")]
    aspect_ratio: String,
    #[serde(rename = "durationSeconds")]
    duration_seconds: u32,
    #[serde(rename = "numberOfVideos")]
    number_of_videos: u32,
}

#[derive(Serialize)]
struct VeoImage {
    #[serde(rename = "imageBytes")]
    image_bytes: String, // base64
    #[serde(rename = "mimeType")]
    mime_type: String,
}

#[derive(Deserialize)]
struct OperationResponse {
    name: Option<String>,
    done: Option<bool>,
    response: Option<VeoResponse>,
    error: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct VeoResponse {
    #[serde(rename = "generatedVideos")]
    generated_videos: Option<Vec<GeneratedVideo>>,
}

#[derive(Deserialize)]
struct GeneratedVideo {
    video: Option<VideoFile>,
}

#[derive(Deserialize)]
struct VideoFile {
    uri: Option<String>,
}

pub struct VideoResult {
    pub video_data: Vec<u8>,
    pub duration: u32,
}

/// Generate a video clip using Veo 3
/// Returns the video bytes (mp4)
pub async fn generate_video(
    api_key: &str,
    prompt: &str,
    duration: u32,
    aspect_ratio: &str,
    reference_image_b64: Option<&str>,
) -> Result<VideoResult, String> {
    let client = reqwest::Client::new();

    // Step 1: Start generation
    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{}:generateVideos?key={}",
        VEO_MODEL, api_key
    );

    let mut body = serde_json::json!({
        "prompt": prompt,
        "config": {
            "aspectRatio": aspect_ratio,
            "durationSeconds": duration.min(8),
            "numberOfVideos": 1
        }
    });

    if let Some(img_b64) = reference_image_b64 {
        body["image"] = serde_json::json!({
            "imageBytes": img_b64,
            "mimeType": "image/jpeg"
        });
    }

    let resp = client.post(&url)
        .json(&body)
        .send().await.map_err(|e| e.to_string())?;

    if !resp.status().is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("Veo API error: {}", &text[..text.len().min(300)]));
    }

    let op: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let op_name = op["name"].as_str().ok_or("No operation name")?.to_string();

    // Step 2: Poll for completion (up to 10 minutes)
    let poll_url = format!(
        "https://generativelanguage.googleapis.com/v1beta/{}?key={}",
        op_name, api_key
    );

    for i in 0..30 {
        tokio::time::sleep(tokio::time::Duration::from_secs(20)).await;

        let poll_resp = client.get(&poll_url)
            .send().await.map_err(|e| e.to_string())?;
        let poll_data: serde_json::Value = poll_resp.json().await.map_err(|e| e.to_string())?;

        if poll_data["done"].as_bool() == Some(true) {
            // Check for video
            if let Some(videos) = poll_data["response"]["generatedVideos"].as_array() {
                if let Some(video) = videos.first() {
                    if let Some(uri) = video["video"]["uri"].as_str() {
                        // Download video
                        let dl_url = format!("{}?key={}", uri, api_key);
                        let video_resp = client.get(&dl_url)
                            .send().await.map_err(|e| e.to_string())?;
                        let video_data = video_resp.bytes().await.map_err(|e| e.to_string())?;
                        return Ok(VideoResult {
                            video_data: video_data.to_vec(),
                            duration,
                        });
                    }
                }
            }
            // Check for error
            if let Some(err) = poll_data.get("error") {
                return Err(format!("Veo generation failed: {}", err));
            }
            return Err("No video in response".into());
        }

        tracing::info!("Veo poll {}/30 for op {}", i+1, &op_name[..op_name.len().min(30)]);
    }

    Err("Veo generation timed out (10 minutes)".into())
}

/// Generate an image using Nano Banana (fast, lightweight)
pub async fn generate_image_nanobanana(
    api_key: &str,
    prompt: &str,
) -> Result<(String, String), String> {
    let client = reqwest::Client::new();
    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
        NANOBANANA_MODEL, api_key
    );

    let body = serde_json::json!({
        "contents": [{"parts": [{"text": prompt}]}],
        "generationConfig": {
            "responseModalities": ["IMAGE", "TEXT"]
        }
    });

    let resp = client.post(&url)
        .json(&body)
        .send().await.map_err(|e| e.to_string())?;

    if !resp.status().is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("Nano Banana error: {}", &text[..text.len().min(300)]));
    }

    let v: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let parts = v["candidates"][0]["content"]["parts"]
        .as_array().ok_or("no parts")?;

    for part in parts {
        if let Some(inline) = part.get("inlineData") {
            let mime = inline["mimeType"].as_str().unwrap_or("image/png").to_string();
            let data = inline["data"].as_str().ok_or("no data")?;
            return Ok((mime, data.to_string()));
        }
    }

    Err("No image in response".into())
}
