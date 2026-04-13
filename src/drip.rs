// ── Drip email scheduler for trial → paid conversion ──
//
// Called every 5 min by cron_scheduler (see main.rs).
// Selects free-plan users whose credits are near zero, who have
// meaningfully used the product (>=3 usage_log rows), and who have
// NOT already received the current template. For each user, builds
// a UserContext, picks an A/B/C variant deterministically from the
// user id, renders the template, sends via send_email(), and logs
// a row into email_campaigns.
//
// Suppression: skips any email present in email_suppression.

use crate::email_templates::{render_depleted_v1, UserContext, TEMPLATE_DEPLETED_V1};
use crate::{send_email, AppState};
use std::sync::Arc;

/// Candidate row pulled by the selector SQL.
struct Candidate {
    user_id: String,
    email: String,
    credits: f64,
}

/// Query the DB for users eligible for the depleted_v1 campaign.
/// Criteria:
///   - plan='free'
///   - credits < 0.5
///   - created_at >= now - 30 days
///   - >= 3 usage_log rows
///   - no existing email_campaigns row for this template
///   - email not in email_suppression
fn select_candidates(
    conn: &rusqlite::Connection,
    template: &str,
) -> rusqlite::Result<Vec<Candidate>> {
    let sql = "
        SELECT u.id, u.email, u.credits
        FROM users u
        WHERE COALESCE(u.plan,'free') = 'free'
          AND u.credits < 0.5
          AND u.created_at >= datetime('now','-30 days')
          AND (SELECT COUNT(*) FROM usage_log ul WHERE ul.user_id = u.id) >= 3
          AND NOT EXISTS (
              SELECT 1 FROM email_campaigns ec
              WHERE ec.user_id = u.id AND ec.template = ?1
          )
          AND u.email IS NOT NULL
          AND NOT EXISTS (
              SELECT 1 FROM email_suppression es WHERE es.email = u.email
          )
        LIMIT 50
    ";
    let mut st = conn.prepare(sql)?;
    let rows = st
        .query_map([template], |r| {
            Ok(Candidate {
                user_id: r.get(0)?,
                email: r.get(1)?,
                credits: r.get(2)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

/// Fetch the context fields for a single user. All queries are best-effort.
fn build_context(
    conn: &rusqlite::Connection,
    cand: &Candidate,
    base_url: &str,
    campaign_id: &str,
) -> UserContext {
    // Recent models (most-recent first, dedup, max 5)
    let recent_models: Vec<String> = conn
        .prepare(
            "SELECT model FROM usage_log WHERE user_id=?1 AND model IS NOT NULL \
             ORDER BY id DESC LIMIT 20",
        )
        .and_then(|mut st| {
            let rows: Vec<String> = st
                .query_map([&cand.user_id], |r| r.get::<_, String>(0))?
                .filter_map(|r| r.ok())
                .collect();
            Ok(rows)
        })
        .unwrap_or_default()
        .into_iter()
        .fold(Vec::new(), |mut acc, m| {
            if !acc.contains(&m) {
                acc.push(m);
            }
            acc
        })
        .into_iter()
        .take(5)
        .collect();

    // Favourite project: most-recent sessions.project
    let fav_project: Option<String> = conn
        .query_row(
            "SELECT project FROM sessions WHERE user_id=?1 AND project IS NOT NULL \
             AND project != '' ORDER BY rowid DESC LIMIT 1",
            [&cand.user_id],
            |r| r.get::<_, String>(0),
        )
        .ok();

    let deployed_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM deployed_apps WHERE user_id=?1",
            [&cand.user_id],
            |r| r.get(0),
        )
        .unwrap_or(0);

    let total_cost_usd: f64 = conn
        .query_row(
            "SELECT COALESCE(SUM(cost_usd),0) FROM usage_log WHERE user_id=?1",
            [&cand.user_id],
            |r| r.get(0),
        )
        .unwrap_or(0.0);

    // Naive language detection: ".jp" email or cjk char in project.
    let lang = if cand.email.ends_with(".jp")
        || fav_project
            .as_deref()
            .unwrap_or("")
            .chars()
            .any(|c| ('\u{3040}'..='\u{9fff}').contains(&c))
    {
        "ja".to_string()
    } else {
        "en".to_string()
    };

    UserContext {
        user_id: cand.user_id.clone(),
        email: cand.email.clone(),
        lang,
        credits: cand.credits,
        recent_models,
        fav_project,
        deployed_count,
        total_cost_usd,
        campaign_id: campaign_id.to_string(),
        base_url: base_url.to_string(),
    }
}

/// Deterministic A/B/C bucketing from user_id.
fn pick_variant(user_id: &str) -> &'static str {
    let sum: u32 = user_id.bytes().map(|b| b as u32).sum();
    match sum % 3 {
        0 => "A",
        1 => "B",
        _ => "C",
    }
}

/// One drip tick. Call every 5 min from the background scheduler.
pub async fn drip_tick(state: &Arc<AppState>) -> Result<(), String> {
    // Snapshot: collect candidates under the lock, drop lock before sending email.
    let (candidates, base_url) = {
        let db = state.db.lock().unwrap_or_else(|e| e.into_inner());
        let list = select_candidates(&db, TEMPLATE_DEPLETED_V1)
            .map_err(|e| format!("select_candidates: {e}"))?;
        (list, state.base_url.clone())
    };

    if candidates.is_empty() {
        return Ok(());
    }

    tracing::info!("drip_tick: {} candidate(s) for {}", candidates.len(), TEMPLATE_DEPLETED_V1);

    for cand in candidates {
        // Build context under a short-lived lock (read-only, lots of queries)
        let campaign_id = uuid::Uuid::new_v4().to_string().replace('-', "")[..16].to_string();
        let variant = pick_variant(&cand.user_id);

        let ctx = {
            let db = state.db.lock().unwrap_or_else(|e| e.into_inner());
            build_context(&db, &cand, &base_url, &campaign_id)
        };

        let (subject, html) = render_depleted_v1(variant, &ctx);

        // INSERT campaign row BEFORE sending, so /r/:id click tracker finds it.
        // If send fails we leave the row but it will never be marked clicked.
        {
            let db = state.db.lock().unwrap_or_else(|e| e.into_inner());
            let now = chrono::Utc::now().to_rfc3339();
            if let Err(e) = db.execute(
                "INSERT INTO email_campaigns (id, user_id, template, variant, sent_at) \
                 VALUES (?1,?2,?3,?4,?5)",
                rusqlite::params![campaign_id, cand.user_id, TEMPLATE_DEPLETED_V1, variant, now],
            ) {
                tracing::warn!("drip_tick: insert campaign failed: {e}");
                continue;
            }
        }

        match send_email(state, &cand.email, &subject, &html, Some(&campaign_id)).await {
            Ok(()) => {
                tracing::info!(
                    "drip_tick: sent {}/{} to {} (variant {})",
                    TEMPLATE_DEPLETED_V1, campaign_id, cand.email, variant
                );
            }
            Err(e) => {
                tracing::warn!("drip_tick: send_email failed for {}: {}", cand.email, e);
            }
        }
    }

    Ok(())
}
