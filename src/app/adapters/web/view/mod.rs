//! HTML rendering helpers for the dashboard (#444).
//!
//! Server-rendered, no template engine. Public functions return `String`
//! suitable for embedding in the larger `dashboard_page` template. Each
//! card has a stable `id="agent-{name}"` so htmx SSE swaps can replace it.

pub mod cards;
pub mod strip;

pub use cards::{agent_card, agent_card_id, agents_section, format_bytes, format_relative};
pub use strip::vps_strip;

/// HTML-escape a string. Internal — kept here so all view functions share one
/// implementation and the templates module's copy doesn't drift.
pub(crate) fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_escape_handles_special_chars() {
        assert_eq!(
            html_escape(r#"<script>alert("x")</script>"#),
            "&lt;script&gt;alert(&quot;x&quot;)&lt;/script&gt;"
        );
    }
}
