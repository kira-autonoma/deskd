//! Integration tests for recurring reminders (#455) and the work_dir-based
//! reminder path resolution (#467).
//!
//! These exercise the scheduler scan loop end-to-end via the public
//! `fire_due_reminders` entrypoint, using a tempdir as the agent's work_dir
//! and an in-process bus listener so we can assert real bus delivery.
//!
//! Each test owns its own tempdir; no process env is mutated and tests can
//! run in parallel without serialization.

use deskd::app::mcp_service::{ScheduleKind, create_reminder_full, parse_schedule_kind};
use deskd::config::RemindDef;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::UnixListener;

fn temp_socket_path() -> String {
    format!("/tmp/deskd-test-recur-{}.sock", uuid::Uuid::new_v4())
}

/// Bind a UnixListener, accept one client, and forward every received `message`
/// payload (`payload.task`) into the returned channel. Stops on first error
/// or when the listener task is dropped.
async fn spawn_bus_listener(socket: &str) -> tokio::sync::mpsc::UnboundedReceiver<String> {
    let listener = UnixListener::bind(socket).expect("bind bus listener");
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(p) => p,
                Err(_) => return,
            };
            let tx = tx.clone();
            tokio::spawn(async move {
                let (reader, _writer) = stream.into_split();
                let mut lines = BufReader::new(reader).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line)
                        && v.get("type").and_then(|t| t.as_str()) == Some("message")
                        && let Some(task) = v
                            .get("payload")
                            .and_then(|p| p.get("task"))
                            .and_then(|t| t.as_str())
                    {
                        let _ = tx.send(task.to_string());
                    }
                }
            });
        }
    });
    rx
}

/// Allocate a fresh agent work_dir with `.deskd/reminders/` pre-created.
fn isolated_work_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("create tempdir");
    std::fs::create_dir_all(dir.path().join(".deskd/reminders")).expect("create reminders dir");
    dir
}

#[test]
fn parse_schedule_kind_mutually_exclusive() {
    let err = parse_schedule_kind(Some("30m"), Some("0 * * * *")).unwrap_err();
    assert!(
        err.to_string().contains("mutually exclusive"),
        "unexpected error: {}",
        err
    );
}

#[test]
fn parse_schedule_kind_interval_below_minute_rejected() {
    let err = parse_schedule_kind(Some("30s"), None).unwrap_err();
    assert!(
        err.to_string().contains(">= 1m"),
        "unexpected error: {}",
        err
    );
}

#[test]
fn parse_schedule_kind_interval_valid() {
    let kind = parse_schedule_kind(Some("2h"), None).unwrap();
    match kind {
        ScheduleKind::Interval(d) => assert_eq!(d.as_secs(), 7200),
        other => panic!("expected Interval, got {:?}", other),
    }
}

#[test]
fn parse_schedule_kind_cron_rejects_six_fields() {
    // 6-field cron implies seconds resolution; deskd is minute-floor only.
    let err = parse_schedule_kind(None, Some("0 0 * * * *")).unwrap_err();
    assert!(
        err.to_string().contains("5 fields"),
        "unexpected error: {}",
        err
    );
}

#[test]
fn parse_schedule_kind_cron_valid_five_field() {
    let kind = parse_schedule_kind(None, Some("*/5 * * * *")).unwrap();
    assert_eq!(kind.label(), "cron");
}

#[test]
fn parse_schedule_kind_none_is_one_shot() {
    let kind = parse_schedule_kind(None, None).unwrap();
    assert_eq!(kind.label(), "one_shot");
}

#[test]
fn next_after_interval_advances_one_step() {
    let now = chrono::Utc::now();
    let last = now;
    let kind = ScheduleKind::Interval(Duration::from_secs(60));
    let next = kind.next_after(last, now).unwrap();
    // One-minute interval: next should be approximately one minute ahead.
    assert_eq!((next - last).num_seconds(), 60);
}

#[test]
fn next_after_interval_collapses_missed_firings() {
    // last_fire was 10 minutes ago, interval is 1m → restart-storm policy
    // should land us at the SINGLE next future tick, not replay 10 ticks.
    let now = chrono::Utc::now();
    let last = now - chrono::Duration::seconds(10 * 60);
    let kind = ScheduleKind::Interval(Duration::from_secs(60));
    let next = kind.next_after(last, now).unwrap();
    assert!(
        next > now,
        "next must be in the future, got {} vs now {}",
        next,
        now
    );
    // And no more than one interval into the future.
    assert!(next <= now + chrono::Duration::seconds(60));
}

#[test]
fn next_after_cron_is_future() {
    // Every minute cron — next occurrence after now is < 60s away and > now.
    let kind = parse_schedule_kind(None, Some("* * * * *")).unwrap();
    let now = chrono::Utc::now();
    let next = kind
        .next_after(now - chrono::Duration::seconds(10), now)
        .unwrap();
    assert!(next > now);
    assert!(next < now + chrono::Duration::seconds(120));
}

/// `create_reminder` with neither `interval` nor `cron_expression` keeps the
/// pre-#455 one-shot semantics: file is written with no recurring fields and
/// schedule_kind reports "one_shot".
#[tokio::test]
async fn create_reminder_one_shot_unchanged() {
    let home = isolated_work_dir();
    let work_dir = home.path();

    let fire_at = chrono::Utc::now() + chrono::Duration::seconds(120);
    let res =
        create_reminder_full(work_dir, "agent:test", "hi", Some(fire_at), None, None).unwrap();
    assert_eq!(res.schedule_kind, "one_shot");

    let dir = deskd::config::reminders_dir_for(work_dir);
    let files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
        .collect();
    assert_eq!(files.len(), 1);

    let content = std::fs::read_to_string(files[0].path()).unwrap();
    let def: RemindDef = serde_json::from_str(&content).unwrap();
    assert!(def.interval.is_none());
    assert!(def.cron_expression.is_none());
}

/// `create_reminder` with `interval` persists the source string so a deskd
/// restart can re-arm without losing the schedule.
#[tokio::test]
async fn create_reminder_interval_persists_source_string() {
    let home = isolated_work_dir();
    let work_dir = home.path();

    let res =
        create_reminder_full(work_dir, "agent:test", "watchdog", None, Some("2h"), None).unwrap();
    assert_eq!(res.schedule_kind, "interval");

    let dir = deskd::config::reminders_dir_for(work_dir);
    let files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
        .collect();
    assert_eq!(files.len(), 1);

    let content = std::fs::read_to_string(files[0].path()).unwrap();
    let def: RemindDef = serde_json::from_str(&content).unwrap();
    assert_eq!(def.interval.as_deref(), Some("2h"));
    assert!(def.cron_expression.is_none());
}

/// `create_reminder` with `cron_expression` persists the source string.
#[tokio::test]
async fn create_reminder_cron_persists_source_string() {
    let home = isolated_work_dir();
    let work_dir = home.path();

    let res = create_reminder_full(
        work_dir,
        "agent:test",
        "watchdog",
        None,
        None,
        Some("0 */2 * * *"),
    )
    .unwrap();
    assert_eq!(res.schedule_kind, "cron");

    let dir = deskd::config::reminders_dir_for(work_dir);
    let files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
        .collect();
    assert_eq!(files.len(), 1);

    let content = std::fs::read_to_string(files[0].path()).unwrap();
    let def: RemindDef = serde_json::from_str(&content).unwrap();
    assert_eq!(def.cron_expression.as_deref(), Some("0 */2 * * *"));
    assert!(def.interval.is_none());
}

/// Mutual-exclusion error is surfaced from `create_reminder_full`.
#[tokio::test]
async fn create_reminder_rejects_both_interval_and_cron() {
    let home = isolated_work_dir();
    let work_dir = home.path();

    let err = create_reminder_full(
        work_dir,
        "agent:test",
        "msg",
        None,
        Some("30m"),
        Some("0 * * * *"),
    )
    .unwrap_err();
    assert!(err.to_string().contains("mutually exclusive"));
}

/// list_reminders reflects schedule_kind and next_fire_at for all three kinds.
#[tokio::test]
async fn list_reminders_reports_schedule_kind() {
    let home = isolated_work_dir();
    let work_dir = home.path();

    create_reminder_full(
        work_dir,
        "agent:a",
        "one-shot",
        Some(chrono::Utc::now() + chrono::Duration::minutes(10)),
        None,
        None,
    )
    .unwrap();
    create_reminder_full(work_dir, "agent:b", "interval", None, Some("1h"), None).unwrap();
    create_reminder_full(work_dir, "agent:c", "cron", None, None, Some("*/5 * * * *")).unwrap();

    let items = deskd::app::mcp_service::list_reminders(work_dir, None, None, None, 50).unwrap();
    assert_eq!(items.len(), 3);

    let kinds: std::collections::HashSet<_> = items
        .iter()
        .map(|v| {
            v.get("schedule_kind")
                .unwrap()
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert!(kinds.contains("one_shot"));
    assert!(kinds.contains("interval"));
    assert!(kinds.contains("cron"));

    // Every entry has next_fire_at populated.
    for v in &items {
        assert!(v.get("next_fire_at").and_then(|x| x.as_str()).is_some());
    }
}

/// Recurring reminder fires twice across two scheduler scans and is never
/// deleted by the scan loop.
#[tokio::test]
async fn interval_reminder_fires_twice_and_persists() {
    let home = isolated_work_dir();
    let work_dir = home.path();

    let socket = temp_socket_path();
    let mut rx = spawn_bus_listener(&socket).await;
    // Give the listener a moment to actually bind.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Write a reminder whose `at` is already in the past, with a 1-minute interval.
    create_reminder_full(
        work_dir,
        "agent:watch",
        "ping",
        Some(chrono::Utc::now() - chrono::Duration::seconds(5)),
        Some("1m"),
        None,
    )
    .unwrap();

    // First scan: should fire and re-arm.
    let now1 = chrono::Utc::now();
    deskd::app::schedule::fire_due_reminders(&socket, "test-agent", work_dir, now1).await;

    let got1 = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("first fire timed out")
        .expect("channel closed");
    assert_eq!(got1, "ping");

    // The reminder file must still exist (recurring keeps the row).
    let dir = deskd::config::reminders_dir_for(work_dir);
    let files1: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
        .collect();
    assert_eq!(files1.len(), 1, "recurring reminder must not be deleted");

    // Force the `at` to be in the past again so the next scan re-fires.
    let path = files1[0].path();
    let content = std::fs::read_to_string(&path).unwrap();
    let mut def: RemindDef = serde_json::from_str(&content).unwrap();
    def.at = (chrono::Utc::now() - chrono::Duration::seconds(1)).to_rfc3339();
    std::fs::write(&path, serde_json::to_string_pretty(&def).unwrap()).unwrap();

    // Second scan: should fire again.
    let now2 = chrono::Utc::now();
    deskd::app::schedule::fire_due_reminders(&socket, "test-agent", work_dir, now2).await;
    let got2 = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("second fire timed out")
        .expect("channel closed");
    assert_eq!(got2, "ping");

    let files2: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
        .collect();
    assert_eq!(files2.len(), 1);
    let _ = std::fs::remove_file(&socket);
}

/// Restart resilience: write a recurring reminder file directly (simulating
/// state on disk after a deskd crash) and confirm the scheduler picks it up
/// on the next scan without any in-memory state.
#[tokio::test]
async fn restart_resilience_picks_up_persisted_recurring() {
    let home = isolated_work_dir();
    let work_dir = home.path();

    let socket = temp_socket_path();
    let mut rx = spawn_bus_listener(&socket).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Simulate a state file already on disk from a previous deskd process.
    let dir = deskd::config::reminders_dir_for(work_dir);
    let path = dir.join("test-fixture.json");
    let def = RemindDef {
        at: (chrono::Utc::now() - chrono::Duration::minutes(2)).to_rfc3339(),
        target: "agent:watch".to_string(),
        message: "from-fixture".to_string(),
        interval: Some("1m".to_string()),
        cron_expression: None,
    };
    std::fs::write(&path, serde_json::to_string_pretty(&def).unwrap()).unwrap();

    let now = chrono::Utc::now();
    deskd::app::schedule::fire_due_reminders(&socket, "test-agent", work_dir, now).await;
    let got = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("fire timed out")
        .expect("channel closed");
    assert_eq!(got, "from-fixture");

    // File still exists with `at` advanced past `now` (restart-storm policy:
    // fire once and move on).
    let content = std::fs::read_to_string(&path).unwrap();
    let updated: RemindDef = serde_json::from_str(&content).unwrap();
    let updated_at: chrono::DateTime<chrono::Utc> =
        chrono::DateTime::parse_from_rfc3339(&updated.at)
            .unwrap()
            .with_timezone(&chrono::Utc);
    assert!(updated_at > now, "re-armed time must be in the future");
    let _ = std::fs::remove_file(&socket);
}

/// cancel_reminder removes the file so a recurring reminder will not fire again.
#[tokio::test]
async fn cancel_recurring_reminder_stops_future_fires() {
    let home = isolated_work_dir();
    let work_dir = home.path();

    let socket = temp_socket_path();
    let mut rx = spawn_bus_listener(&socket).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Create + cancel before the scheduler runs.
    create_reminder_full(
        work_dir,
        "agent:watch",
        "should-not-fire",
        Some(chrono::Utc::now() - chrono::Duration::seconds(5)),
        Some("1m"),
        None,
    )
    .unwrap();

    let dir = deskd::config::reminders_dir_for(work_dir);
    let files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
        .collect();
    let id = files[0]
        .path()
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .to_string();

    let res = deskd::app::mcp_service::cancel_reminder(work_dir, &id).unwrap();
    assert_eq!(
        res.get("cancelled").unwrap(),
        &serde_json::Value::Bool(true)
    );

    // Now a scheduler scan must NOT fire anything.
    deskd::app::schedule::fire_due_reminders(&socket, "test-agent", work_dir, chrono::Utc::now())
        .await;

    let timeout = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await;
    assert!(
        timeout.is_err(),
        "cancelled reminder must not produce bus message"
    );
    let _ = std::fs::remove_file(&socket);
}

/// #467 regression: two agents with different work_dirs must have ISOLATED
/// reminder directories. A reminder created for agent A must never be
/// picked up by the scanner running with agent B's work_dir.
#[tokio::test]
async fn reminders_are_isolated_per_work_dir() {
    let home_a = isolated_work_dir();
    let home_b = isolated_work_dir();

    // Write a due reminder under agent A's work_dir.
    create_reminder_full(
        home_a.path(),
        "agent:a",
        "a-only",
        Some(chrono::Utc::now() - chrono::Duration::seconds(5)),
        Some("1m"),
        None,
    )
    .unwrap();

    // Agent B's bus listener — should never receive A's reminder.
    let socket_b = temp_socket_path();
    let mut rx_b = spawn_bus_listener(&socket_b).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Run B's scanner against B's work_dir.
    deskd::app::schedule::fire_due_reminders(
        &socket_b,
        "agent-b",
        home_b.path(),
        chrono::Utc::now(),
    )
    .await;

    let timeout = tokio::time::timeout(Duration::from_millis(300), rx_b.recv()).await;
    assert!(
        timeout.is_err(),
        "agent B saw agent A's reminder — work_dir isolation broken"
    );

    // Now run A's scanner against A's work_dir — it MUST deliver.
    let socket_a = temp_socket_path();
    let mut rx_a = spawn_bus_listener(&socket_a).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    deskd::app::schedule::fire_due_reminders(
        &socket_a,
        "agent-a",
        home_a.path(),
        chrono::Utc::now(),
    )
    .await;
    let got = tokio::time::timeout(Duration::from_secs(2), rx_a.recv())
        .await
        .expect("agent A scanner did not deliver own reminder")
        .expect("channel closed");
    assert_eq!(got, "a-only");

    let _ = std::fs::remove_file(&socket_a);
    let _ = std::fs::remove_file(&socket_b);
}

/// #467 end-to-end: a reminder written to `{work_dir}/.deskd/reminders/` by
/// `create_reminder_full` must be discovered and delivered by
/// `fire_due_reminders` when given the same `work_dir`. This is the integration
/// test required by the issue's acceptance criteria.
#[tokio::test]
async fn create_reminder_fires_through_scanner_with_work_dir() {
    let home = isolated_work_dir();
    let work_dir = home.path();

    let socket = temp_socket_path();
    let mut rx = spawn_bus_listener(&socket).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Past `at` so the first scan immediately fires.
    create_reminder_full(
        work_dir,
        "agent:watch",
        "tick",
        Some(chrono::Utc::now() - chrono::Duration::seconds(1)),
        Some("1m"),
        None,
    )
    .unwrap();

    deskd::app::schedule::fire_due_reminders(&socket, "agent-watch", work_dir, chrono::Utc::now())
        .await;

    let got = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("reminder did not fire")
        .expect("channel closed");
    assert_eq!(got, "tick");

    // The file must still exist (recurring re-arm).
    let dir = deskd::config::reminders_dir_for(work_dir);
    let files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
        .collect();
    assert_eq!(
        files.len(),
        1,
        "recurring reminder must be re-armed, not deleted"
    );

    let _ = std::fs::remove_file(&socket);
}
