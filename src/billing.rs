use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Per-user rate limiter: max N requests per minute
pub struct RateLimiter {
    windows: Mutex<HashMap<String, Vec<Instant>>>,
    max_per_minute: usize,
}

impl RateLimiter {
    pub fn new(max_per_minute: usize) -> Self {
        Self { windows: Mutex::new(HashMap::new()), max_per_minute }
    }

    pub fn check(&self, user_id: &str) -> bool {
        let mut windows = self.windows.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let entry = windows.entry(user_id.to_string()).or_default();

        // Remove entries older than 60s
        entry.retain(|t| now.duration_since(*t).as_secs() < 60);

        if entry.len() >= self.max_per_minute {
            return false; // rate limited
        }
        entry.push(now);
        true
    }
}

/// Stripe Checkout session creation
pub async fn create_checkout_session(
    stripe_key: &str,
    user_email: &str,
    user_token: &str,
    amount_credits: f64,
    base_url: &str,
) -> Result<String, String> {
    let price_usd = (amount_credits * 100.0) as i64; // cents

    let client = reqwest::Client::new();
    let params = [
        ("mode", "payment"),
        ("success_url", &format!("{}/billing/success?token={}", base_url, user_token)),
        ("cancel_url", &format!("{}/billing/cancel", base_url)),
        ("customer_email", user_email),
        ("line_items[0][price_data][currency]", "usd"),
        ("line_items[0][price_data][product_data][name]", "Claude Code Credits"),
        ("line_items[0][price_data][unit_amount]", &price_usd.to_string()),
        ("line_items[0][quantity]", "1"),
        ("metadata[user_token]", user_token),
        ("metadata[credits]", &amount_credits.to_string()),
    ];

    let resp = client.post("https://api.stripe.com/v1/checkout/sessions")
        .header("Authorization", format!("Bearer {}", stripe_key))
        .form(&params)
        .send()
        .await
        .map_err(|e| e.to_string())?;

    let body: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;

    if let Some(url) = body.get("url").and_then(|u| u.as_str()) {
        Ok(url.to_string())
    } else {
        Err(body.get("error").and_then(|e| e.get("message")).and_then(|m| m.as_str())
            .unwrap_or("Unknown Stripe error").to_string())
    }
}

/// Verify Stripe webhook and extract credit amount
pub fn parse_webhook_event(body: &str) -> Option<(String, f64)> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let event_type = v.get("type")?.as_str()?;
    if event_type != "checkout.session.completed" { return None; }

    let session = v.get("data")?.get("object")?;
    let metadata = session.get("metadata")?;
    let user_token = metadata.get("user_token")?.as_str()?;
    let credits: f64 = metadata.get("credits")?.as_str()?.parse().ok()?;

    Some((user_token.to_string(), credits))
}
