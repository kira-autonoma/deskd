//! Inline HTML templates for the web adapter (#443).
//!
//! Kept minimal on purpose — the dashboard is a placeholder that future
//! children of #442 will replace. CSP forbids inline `<script>` tags so all
//! interactivity must come from same-origin assets (none today).

/// Render the `/login` page with a single button. The CSRF field is required
/// even though the button has no real form fields beyond `_csrf` because the
/// CSRF middleware checks every POST.
pub fn login_page(csrf: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>deskd · login</title>
<meta name="viewport" content="width=device-width, initial-scale=1">
</head>
<body>
<main>
  <h1>deskd</h1>
  <p>Sign in via Telegram. We will send a one-time link to your configured account.</p>
  <form method="post" action="/login/request">
    <input type="hidden" name="_csrf" value="{csrf}">
    <button type="submit">Send link to Telegram</button>
  </form>
</main>
</body>
</html>"#,
        csrf = html_escape(csrf)
    )
}

/// Render the dashboard (#444). Mobile-first, server-rendered. Live updates
/// arrive over SSE via vendored htmx + the htmx-ext-sse extension; both are
/// served from `/static/*` to keep the strict CSP from #443 (`script-src
/// 'self'`).
pub fn dashboard_page(
    telegram_id: i64,
    csrf: &str,
    vps_strip_html: &str,
    agents_section_html: &str,
) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>deskd · dashboard</title>
<meta name="viewport" content="width=device-width, initial-scale=1">
<style>{css}</style>
<script src="/static/htmx.min.js"></script>
<script src="/static/htmx-sse.js"></script>
</head>
<body>
<header class="topbar">
  <h1>deskd</h1>
  <span class="topbar__user">tg:{telegram_id}</span>
  <form class="topbar__logout" method="post" action="/logout">
    <input type="hidden" name="_csrf" value="{csrf}">
    <button type="submit">Log out</button>
  </form>
</header>
<main>
  {vps_strip_html}
  {agents_section_html}
</main>
</body>
</html>"#,
        telegram_id = telegram_id,
        csrf = html_escape(csrf),
        css = DASHBOARD_CSS,
        vps_strip_html = vps_strip_html,
        agents_section_html = agents_section_html,
    )
}

/// Inline CSS for the dashboard. Mobile-first; kept tiny on purpose. CSP
/// permits `style-src 'self'` so an inline `<style>` block (same-origin
/// document) is allowed.
const DASHBOARD_CSS: &str = r#"
* { box-sizing: border-box; }
body { margin: 0; font: 14px/1.45 -apple-system, BlinkMacSystemFont, "Segoe UI", system-ui, sans-serif; color: #1a1a1a; background: #f6f7fa; }
.topbar { display: flex; align-items: center; gap: 0.6rem; padding: 0.5rem 1rem; background: #1a1a1a; color: #f6f7fa; }
.topbar h1 { margin: 0; font-size: 1.1rem; flex: 0 0 auto; }
.topbar__user { font-size: 0.8rem; opacity: 0.7; flex: 1 1 auto; }
.topbar__logout { margin: 0; }
.topbar__logout button { background: transparent; color: inherit; border: 1px solid #555; border-radius: 4px; padding: 0.25rem 0.6rem; font: inherit; }
main { padding: 0.75rem; max-width: 720px; margin: 0 auto; }
.vps-strip { display: flex; flex-wrap: wrap; gap: 0.5rem 1rem; padding: 0.6rem 0.75rem; margin-bottom: 0.75rem; background: #fff; border: 1px solid #e0e3eb; border-radius: 8px; font-size: 0.85rem; }
.vps-strip__item strong { color: #555; font-weight: 600; margin-right: 0.25rem; }
.agents { display: flex; flex-direction: column; gap: 0.6rem; }
.agents--empty .agents__empty { padding: 1.5rem 1rem; text-align: center; background: #fff; border: 1px dashed #c5c9d4; border-radius: 8px; color: #666; }
.agent-card { background: #fff; border: 1px solid #e0e3eb; border-radius: 8px; padding: 0.75rem 0.9rem; }
.agent-card__head { display: flex; flex-wrap: wrap; align-items: center; gap: 0.4rem 0.6rem; margin-bottom: 0.4rem; }
.agent-card__name { margin: 0; font-size: 1rem; font-weight: 600; }
.agent-card__status { font-size: 0.75rem; padding: 0.1rem 0.45rem; border-radius: 999px; font-weight: 600; text-transform: uppercase; letter-spacing: 0.04em; }
.agent-card__status--idle { background: #eef0f5; color: #555; }
.agent-card__status--working { background: #e6f4ea; color: #1e7e34; }
.agent-card__status--offline { background: #fbeaea; color: #b02929; }
.agent-card__model { font-size: 0.75rem; color: #777; margin-left: auto; font-family: ui-monospace, "SF Mono", Menlo, monospace; }
.agent-card__rows { margin: 0; display: grid; grid-template-columns: max-content 1fr; gap: 0.2rem 0.75rem; font-size: 0.85rem; }
.agent-card__rows dt { color: #666; }
.agent-card__rows dd { margin: 0; }
.ctx-numbers { font-variant-numeric: tabular-nums; }
.ctx-bar { display: inline-block; width: 100px; height: 6px; background: #e0e3eb; border-radius: 3px; vertical-align: middle; margin: 0 0.4rem; overflow: hidden; }
.ctx-bar__fill { display: block; height: 100%; background: #4a72d9; transition: width 0.3s ease; }
.ctx-pct { font-variant-numeric: tabular-nums; color: #555; }
.em { color: #aaa; }
small { color: #777; }
@media (max-width: 420px) {
  .agent-card__rows { grid-template-columns: 1fr; gap: 0.05rem 0; }
  .agent-card__rows dt { font-size: 0.7rem; text-transform: uppercase; letter-spacing: 0.04em; margin-top: 0.3rem; }
  .ctx-bar { width: 80px; }
}
"#;

/// Page shown after a successful magic-link request: tells the user the link
/// has been dispatched. Kept on a separate URL so a refresh doesn't re-POST.
pub fn link_sent_page() -> &'static str {
    r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>deskd · link sent</title>
<meta name="viewport" content="width=device-width, initial-scale=1">
</head>
<body>
<main>
  <h1>Login link sent</h1>
  <p>Check Telegram for a one-time login link. It is single-use and expires shortly.</p>
</main>
</body>
</html>"#
}

fn html_escape(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '&' => "&amp;".into(),
            '<' => "&lt;".into(),
            '>' => "&gt;".into(),
            '"' => "&quot;".into(),
            '\'' => "&#39;".into(),
            other => other.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_page_includes_csrf_token() {
        let html = login_page("token-xyz");
        assert!(html.contains(r#"value="token-xyz""#));
        assert!(html.contains(r#"action="/login/request""#));
    }

    #[test]
    fn login_page_escapes_csrf_token() {
        // Defense in depth — even though tokens are base64url they go through
        // the escaper. Use an XSS-shaped fake input to verify.
        let html = login_page("<script>alert(1)</script>");
        assert!(!html.contains("<script>alert(1)</script>"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn dashboard_page_includes_logout_form() {
        let html = dashboard_page(
            42,
            "csrf-1",
            "<section class='vps-strip'></section>",
            "<section></section>",
        );
        assert!(html.contains("tg:42"));
        assert!(html.contains(r#"action="/logout""#));
        assert!(html.contains(r#"value="csrf-1""#));
    }

    #[test]
    fn dashboard_page_loads_vendored_htmx() {
        let html = dashboard_page(1, "x", "", "");
        // Vendored under /static/ — never reach out to a CDN, keeps strict
        // CSP (script-src 'self') intact.
        assert!(html.contains(r#"src="/static/htmx.min.js""#));
        assert!(html.contains(r#"src="/static/htmx-sse.js""#));
    }

    #[test]
    fn dashboard_page_includes_word_dashboard_for_smoke_tests() {
        // The existing #443 integration test asserts on the literal word
        // "dashboard" appearing in the HTML; preserve that.
        let html = dashboard_page(1, "x", "", "");
        assert!(html.contains("dashboard"));
    }
}
