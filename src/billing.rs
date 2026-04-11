use std::collections::HashMap;
use std::sync::Mutex;
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
        entry.retain(|t| now.duration_since(*t).as_secs() < 60);
        if entry.len() >= self.max_per_minute {
            return false;
        }
        entry.push(now);
        true
    }
}

/// Credits granted per plan per billing cycle
pub fn plan_credits(plan: &str) -> f64 {
    match plan {
        "starter" => 10.0,
        "pro"     => 35.0,
        "power"   => 100.0,
        _         => 0.0,
    }
}

/// Stripe Checkout session (one-time payment)
pub async fn create_checkout_session(
    stripe_key: &str,
    user_email: &str,
    user_token: &str,
    amount_credits: f64,
    base_url: &str,
) -> Result<String, String> {
    let price_usd = (amount_credits * 100.0) as i64;
    let price_str = price_usd.to_string();
    let success_url = format!("{}/billing/success?token={}", base_url, user_token);
    let cancel_url = format!("{}/billing/cancel", base_url);
    let credits_str = amount_credits.to_string();

    let params: Vec<(&str, &str)> = vec![
        ("mode", "payment"),
        ("success_url", &success_url),
        ("cancel_url", &cancel_url),
        ("customer_email", user_email),
        ("line_items[0][price_data][currency]", "usd"),
        ("line_items[0][price_data][product_data][name]", "ChatWeb Credits"),
        ("line_items[0][price_data][unit_amount]", &price_str),
        ("line_items[0][quantity]", "1"),
        ("metadata[user_token]", user_token),
        ("metadata[credits]", &credits_str),
    ];

    stripe_post(stripe_key, params).await
}

/// Stripe Checkout session (monthly subscription)
pub async fn create_subscription_checkout(
    stripe_key: &str,
    user_email: &str,
    user_token: &str,
    plan: &str,
    base_url: &str,
) -> Result<String, String> {
    let (amount_cents, plan_name) = match plan {
        "starter" => (900i64,  "ChatWeb Starter — 10 credits/month"),
        "pro"     => (2900i64, "ChatWeb Pro — 35 credits/month"),
        "power"   => (7900i64, "ChatWeb Power — 100 credits/month"),
        _         => return Err("Unknown plan".to_string()),
    };

    let amount_str = amount_cents.to_string();
    let success_url = format!("{}/billing/success?token={}", base_url, user_token);
    let cancel_url = format!("{}/billing/cancel", base_url);

    let params: Vec<(&str, &str)> = vec![
        ("mode", "subscription"),
        ("success_url", &success_url),
        ("cancel_url", &cancel_url),
        ("customer_email", user_email),
        ("line_items[0][price_data][currency]", "usd"),
        ("line_items[0][price_data][product_data][name]", plan_name),
        ("line_items[0][price_data][unit_amount]", &amount_str),
        ("line_items[0][price_data][recurring][interval]", "month"),
        ("line_items[0][quantity]", "1"),
        ("metadata[user_token]", user_token),
        ("metadata[plan]", plan),
        ("subscription_data[metadata][user_token]", user_token),
        ("subscription_data[metadata][plan]", plan),
    ];

    stripe_post(stripe_key, params).await
}

async fn stripe_post(stripe_key: &str, params: Vec<(&str, &str)>) -> Result<String, String> {
    let client = reqwest::Client::new();
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
        Err(body.get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or("Unknown Stripe error").to_string())
    }
}

/// Webhook action parsed from a Stripe event body
pub enum WebhookAction {
    /// One-time checkout payment completed
    OneTimeCredits { token: String, credits: f64 },
    /// New subscription started via checkout
    SubscriptionStarted { token: String, plan: String, customer_id: String },
    /// Monthly renewal (plan is stored in DB; look up by customer_id)
    SubscriptionRenewed { customer_id: String },
}

pub fn parse_webhook_action(body: &str) -> Option<WebhookAction> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let event_type = v.get("type")?.as_str()?;

    match event_type {
        "checkout.session.completed" => {
            let session  = v.get("data")?.get("object")?;
            let metadata = session.get("metadata")?;
            let token    = metadata.get("user_token")?.as_str()?.to_string();
            let mode     = session.get("mode")?.as_str()?;

            if mode == "payment" {
                let credits: f64 = metadata.get("credits")?.as_str()?.parse().ok()?;
                Some(WebhookAction::OneTimeCredits { token, credits })
            } else if mode == "subscription" {
                let plan = metadata.get("plan")?.as_str()?.to_string();
                let customer_id = session.get("customer")
                    .and_then(|c| c.as_str()).unwrap_or("").to_string();
                Some(WebhookAction::SubscriptionStarted { token, plan, customer_id })
            } else {
                None
            }
        }
        "invoice.payment_succeeded" => {
            let invoice        = v.get("data")?.get("object")?;
            let billing_reason = invoice.get("billing_reason")?.as_str()?;
            // Skip first-invoice — already handled by checkout.session.completed
            if billing_reason == "subscription_create" { return None; }
            let customer_id = invoice.get("customer")?.as_str()?.to_string();
            Some(WebhookAction::SubscriptionRenewed { customer_id })
        }
        _ => None,
    }
}
