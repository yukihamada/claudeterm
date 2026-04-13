// ── Email Templates for Trial → Paid Conversion Drip ──
// Three A/B/C variants (practical / social proof / urgency) with ja/en.
// Each template returns (subject, html). HTML uses inline CSS and is
// dark-mode friendly (explicit dark background + light text).

/// Context passed to every template. Keep fields lightweight —
/// the drip loop computes these from users/usage_log/deployed_apps.
pub struct UserContext {
    pub user_id: String,
    pub email: String,
    pub lang: String,             // "ja" or "en"
    pub credits: f64,
    pub recent_models: Vec<String>,
    pub fav_project: Option<String>,
    pub deployed_count: i64,
    pub total_cost_usd: f64,
    /// Tracking link id — the /r/:id endpoint records the click and redirects.
    pub campaign_id: String,
    pub base_url: String,
}

impl UserContext {
    fn cta_href(&self, to: &str) -> String {
        // Click tracker: /r/:id?to=/topup
        format!(
            "{}/r/{}?to={}",
            self.base_url,
            self.campaign_id,
            urlencoding::encode(to),
        )
    }
}

/// Shared shell wrapping content. Dark-mode friendly.
fn shell(title: &str, body: &str) -> String {
    format!(
        r#"<!DOCTYPE html><html><head><meta charset="utf-8"><title>{title}</title></head>
<body style="margin:0;padding:0;background:#09090b;color:#e4e4e7;font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',system-ui,sans-serif;-webkit-font-smoothing:antialiased">
  <div style="max-width:520px;margin:0 auto;padding:40px 24px">
    <div style="display:flex;align-items:center;gap:8px;margin-bottom:24px">
      <div style="width:36px;height:36px;border-radius:10px;background:linear-gradient(135deg,#a78bfa,#60a5fa)"></div>
      <div style="font-size:18px;font-weight:700;color:#fafafa">ChatWeb</div>
    </div>
    <div style="background:#18181b;border:1px solid #27272a;border-radius:16px;padding:28px">
      {body}
    </div>
    <p style="color:#52525b;font-size:11px;line-height:1.6;margin:24px 0 0;text-align:center">
      chatweb.ai &middot; <a href="https://chatweb.ai" style="color:#71717a;text-decoration:none">chatweb.ai</a><br>
      You received this because you signed up for ChatWeb.
    </p>
  </div>
</body></html>"#
    )
}

fn btn(href: &str, label: &str) -> String {
    format!(
        r#"<div style="margin:24px 0"><a href="{href}" style="display:inline-block;background:#a78bfa;color:#09090b;font-weight:700;font-size:14px;padding:14px 24px;border-radius:10px;text-decoration:none">{label}</a></div>"#
    )
}

fn recent_list(models: &[String]) -> String {
    if models.is_empty() {
        return String::new();
    }
    let items: String = models
        .iter()
        .take(5)
        .map(|m| format!(r#"<li style="color:#a1a1aa;font-size:13px;margin:4px 0">{m}</li>"#))
        .collect();
    format!(r#"<ul style="padding-left:20px;margin:12px 0">{items}</ul>"#)
}

// ── Variant A: practical ─────────────────────────────────────────────

pub fn template_depleted_v1_a(ctx: &UserContext) -> (String, String) {
    let cta = ctx.cta_href("/?view=topup");
    let fav = ctx
        .fav_project
        .clone()
        .unwrap_or_else(|| if ctx.lang == "ja" { "プロジェクト".into() } else { "your project".into() });
    let recents = recent_list(&ctx.recent_models);

    if ctx.lang == "ja" {
        let subject = "クレジット残りわずかです — そのまま続けますか?".to_string();
        let body = format!(
r#"<h2 style="color:#fafafa;font-size:20px;margin:0 0 12px">クレジットが残り少なくなっています</h2>
<p style="color:#a1a1aa;font-size:14px;line-height:1.6;margin:0 0 12px">
現在の残高: <b style="color:#fafafa">${credits:.2}</b><br>
これまでに「<b style="color:#e4e4e7">{fav}</b>」で作業中ですね。</p>
<p style="color:#a1a1aa;font-size:13px;margin:12px 0 4px">最近使ったモデル:</p>
{recents}
<p style="color:#a1a1aa;font-size:14px;line-height:1.6;margin:16px 0 0">
$10 チャージすれば、Claude Sonnet でそのまま同じ作業を続けられます。
初回チャージはクレジットが失効しません。</p>
{btn}"#,
            credits = ctx.credits,
            fav = fav,
            recents = recents,
            btn = btn(&cta, "$10 チャージして続ける →"),
        );
        (subject, shell("クレジットを追加", &body))
    } else {
        let subject = "Your credits are almost out — keep going?".to_string();
        let body = format!(
r#"<h2 style="color:#fafafa;font-size:20px;margin:0 0 12px">Your credits are running low</h2>
<p style="color:#a1a1aa;font-size:14px;line-height:1.6;margin:0 0 12px">
Current balance: <b style="color:#fafafa">${credits:.2}</b><br>
You've been working on <b style="color:#e4e4e7">{fav}</b>.</p>
<p style="color:#a1a1aa;font-size:13px;margin:12px 0 4px">Recent models:</p>
{recents}
<p style="color:#a1a1aa;font-size:14px;line-height:1.6;margin:16px 0 0">
Top up $10 to pick up exactly where you left off with Claude Sonnet.
Credits never expire on your first top-up.</p>
{btn}"#,
            credits = ctx.credits,
            fav = fav,
            recents = recents,
            btn = btn(&cta, "Top up $10 & continue →"),
        );
        (subject, shell("Top up credits", &body))
    }
}

// ── Variant B: social proof ──────────────────────────────────────────

pub fn template_depleted_v1_b(ctx: &UserContext) -> (String, String) {
    let cta = ctx.cta_href("/?view=topup");

    if ctx.lang == "ja" {
        let subject = "100 人以上が ChatWeb Pro を選んだ理由".to_string();
        let body = format!(
r#"<h2 style="color:#fafafa;font-size:20px;margin:0 0 12px">100 人以上のビルダーに選ばれています</h2>
<p style="color:#a1a1aa;font-size:14px;line-height:1.6;margin:0 0 16px">
ChatWeb を試していただきありがとうございます。
現在 <b style="color:#fafafa">${credits:.2}</b> のクレジットが残っています。</p>
<div style="background:#0f0f10;border-left:3px solid #a78bfa;padding:14px 16px;margin:16px 0;border-radius:6px">
  <p style="color:#d4d4d8;font-size:14px;font-style:italic;margin:0">
    「朝のアイデアを昼までにデプロイできる速度に驚いた。
    Pro プランはすぐ元が取れた。」</p>
  <p style="color:#71717a;font-size:12px;margin:8px 0 0">— 実際の ChatWeb Pro ユーザー</p>
</div>
<p style="color:#a1a1aa;font-size:14px;line-height:1.6;margin:0 0 0">
Pro プラン ($29/月, 35 クレジット) なら、平均 3.5 倍のプロジェクトを
同じ月内に完成させられます。</p>
{btn}"#,
            credits = ctx.credits,
            btn = btn(&cta, "Pro プランを見る →"),
        );
        (subject, shell("Pro にアップグレード", &body))
    } else {
        let subject = "Why 100+ builders chose ChatWeb Pro".to_string();
        let body = format!(
r#"<h2 style="color:#fafafa;font-size:20px;margin:0 0 12px">Join 100+ builders on Pro</h2>
<p style="color:#a1a1aa;font-size:14px;line-height:1.6;margin:0 0 16px">
Thanks for trying ChatWeb. You have <b style="color:#fafafa">${credits:.2}</b> credits remaining.</p>
<div style="background:#0f0f10;border-left:3px solid #a78bfa;padding:14px 16px;margin:16px 0;border-radius:6px">
  <p style="color:#d4d4d8;font-size:14px;font-style:italic;margin:0">
    "I ship an idea from breakfast to deploy before lunch.
    Pro paid for itself in a week."</p>
  <p style="color:#71717a;font-size:12px;margin:8px 0 0">— Actual ChatWeb Pro user</p>
</div>
<p style="color:#a1a1aa;font-size:14px;line-height:1.6;margin:0 0 0">
Pro ($29/mo, 35 credits) users ship <b>3.5&times; more projects</b>
in the same month compared to top-up-only users.</p>
{btn}"#,
            credits = ctx.credits,
            btn = btn(&cta, "See Pro plan →"),
        );
        (subject, shell("Upgrade to Pro", &body))
    }
}

// ── Variant C: urgency ───────────────────────────────────────────────

pub fn template_depleted_v1_c(ctx: &UserContext) -> (String, String) {
    let cta = ctx.cta_href("/?view=topup&promo=FIRST20");

    if ctx.lang == "ja" {
        let subject = "【今週だけ】初回チャージ 20% OFF".to_string();
        let body = format!(
r#"<h2 style="color:#fafafa;font-size:20px;margin:0 0 12px">初回チャージ 20% OFF — 今週だけ</h2>
<p style="color:#a1a1aa;font-size:14px;line-height:1.6;margin:0 0 12px">
残高: <b style="color:#fafafa">${credits:.2}</b>。もうすぐ使い切れそうですね。</p>
<div style="background:linear-gradient(135deg,rgba(167,139,250,.1),rgba(96,165,250,.1));border:1px solid #3f3f46;border-radius:12px;padding:18px;margin:16px 0;text-align:center">
  <div style="font-size:32px;font-weight:800;color:#a78bfa;letter-spacing:2px">20% OFF</div>
  <div style="color:#a1a1aa;font-size:12px;margin-top:4px">プロモコード: <b style="color:#fafafa">FIRST20</b></div>
  <div style="color:#71717a;font-size:11px;margin-top:6px">7 日間限定 &middot; 初回チャージのみ</div>
</div>
<p style="color:#a1a1aa;font-size:14px;line-height:1.6;margin:0">
$10 チャージが実質 <b style="color:#fafafa">$8</b>。
今すぐ使えば、本日中にもう 1 つプロジェクトを仕上げられます。</p>
{btn}"#,
            credits = ctx.credits,
            btn = btn(&cta, "20% OFF で続ける →"),
        );
        (subject, shell("初回 20% OFF", &body))
    } else {
        let subject = "[This week only] 20% off your first top-up".to_string();
        let body = format!(
r#"<h2 style="color:#fafafa;font-size:20px;margin:0 0 12px">20% off your first top-up — this week only</h2>
<p style="color:#a1a1aa;font-size:14px;line-height:1.6;margin:0 0 12px">
Balance: <b style="color:#fafafa">${credits:.2}</b>. Nearly out.</p>
<div style="background:linear-gradient(135deg,rgba(167,139,250,.1),rgba(96,165,250,.1));border:1px solid #3f3f46;border-radius:12px;padding:18px;margin:16px 0;text-align:center">
  <div style="font-size:32px;font-weight:800;color:#a78bfa;letter-spacing:2px">20% OFF</div>
  <div style="color:#a1a1aa;font-size:12px;margin-top:4px">Promo code: <b style="color:#fafafa">FIRST20</b></div>
  <div style="color:#71717a;font-size:11px;margin-top:6px">7 days only &middot; first top-up only</div>
</div>
<p style="color:#a1a1aa;font-size:14px;line-height:1.6;margin:0">
A $10 top-up is effectively <b style="color:#fafafa">$8</b>.
Enough to ship one more idea today.</p>
{btn}"#,
            credits = ctx.credits,
            btn = btn(&cta, "Claim 20% off →"),
        );
        (subject, shell("First top-up 20% off", &body))
    }
}

/// Dispatch by variant letter ("A"|"B"|"C") to avoid duplication at call sites.
pub fn render_depleted_v1(variant: &str, ctx: &UserContext) -> (String, String) {
    match variant {
        "A" => template_depleted_v1_a(ctx),
        "B" => template_depleted_v1_b(ctx),
        _   => template_depleted_v1_c(ctx),
    }
}

pub const TEMPLATE_DEPLETED_V1: &str = "depleted_v1";
