//! VPS overview strip rendering (#444).
//!
//! Top-of-page strip showing deskd version, uptime, and disk metrics. Disk
//! comes from #446 once landed; we render an em-dash placeholder until then.

use std::time::Duration;

use crate::app::adapters::web::data::VpsOverview;

use super::{cards::format_bytes, html_escape};

/// Render the VPS overview strip as an HTML fragment. Designed to fit a
/// 375px-wide phone viewport without horizontal scroll.
pub fn vps_strip(overview: &VpsOverview) -> String {
    let version = html_escape(&overview.deskd_version);
    let uptime = overview.uptime.map(format_uptime).unwrap_or_else(em_dash);
    let disk_label = match (overview.disk_free_bytes, overview.disk_total_bytes) {
        (Some(free), Some(total)) if total > 0 => {
            format!("{} free / {}", format_bytes(free), format_bytes(total),)
        }
        _ => em_dash(),
    };

    format!(
        r#"<section class="vps-strip">
  <span class="vps-strip__item"><strong>deskd</strong> v{version}</span>
  <span class="vps-strip__item"><strong>uptime</strong> {uptime}</span>
  <span class="vps-strip__item"><strong>disk</strong> {disk_label}</span>
</section>"#,
        version = version,
        uptime = uptime,
        disk_label = disk_label,
    )
}

fn em_dash() -> String {
    "<span class=\"em\">—</span>".to_string()
}

/// Format an uptime as `5d 3h`, `2h 14m`, `45m`, `12s`. Days take precedence
/// once we cross 24 hours; minute-precision stops at the day boundary.
fn format_uptime(d: Duration) -> String {
    let total = d.as_secs();
    if total < 60 {
        return format!("{}s", total);
    }
    let mins = total / 60;
    if mins < 60 {
        return format!("{}m", mins);
    }
    let hours = mins / 60;
    let rem_min = mins % 60;
    if hours < 24 {
        if rem_min == 0 {
            return format!("{}h", hours);
        }
        return format!("{}h {}m", hours, rem_min);
    }
    let days = hours / 24;
    let rem_h = hours % 24;
    if rem_h == 0 {
        format!("{}d", days)
    } else {
        format!("{}d {}h", days, rem_h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overview() -> VpsOverview {
        VpsOverview {
            deskd_version: "0.1.2".into(),
            uptime: Some(Duration::from_secs(3600 + 600)), // 1h 10m
            disk_total_bytes: None,
            disk_free_bytes: None,
        }
    }

    #[test]
    fn strip_includes_version_and_uptime() {
        let html = vps_strip(&overview());
        assert!(html.contains("v0.1.2"));
        assert!(html.contains("1h 10m"));
    }

    #[test]
    fn strip_renders_em_dash_for_missing_disk() {
        let html = vps_strip(&overview());
        assert!(html.contains("disk"));
        assert!(html.contains("—"));
    }

    #[test]
    fn strip_renders_disk_when_available() {
        let mut o = overview();
        o.disk_free_bytes = Some(40 * 1024 * 1024 * 1024);
        o.disk_total_bytes = Some(80 * 1024 * 1024 * 1024);
        let html = vps_strip(&o);
        assert!(html.contains("40.00 GiB free / 80.00 GiB"));
    }

    #[test]
    fn format_uptime_buckets() {
        assert_eq!(format_uptime(Duration::from_secs(45)), "45s");
        assert_eq!(format_uptime(Duration::from_secs(60 * 14)), "14m");
        assert_eq!(format_uptime(Duration::from_secs(3600)), "1h");
        assert_eq!(format_uptime(Duration::from_secs(3600 * 26)), "1d 2h");
        assert_eq!(format_uptime(Duration::from_secs(3600 * 24 * 5)), "5d");
    }
}
