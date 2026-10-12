//! The scheduler on the real clock: a queue scheduled for the current minute starts by
//! itself, downloads, reports its "when finished" action, and (run-once) switches off.

use chrono::{Local, Timelike};
use rdm_core::settings::{MAIN_QUEUE, OnQueueDone};
use rdm_core::{AddOptions, DownloadRequest, Event, ItemState, Manager};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn serve(body: Vec<u8>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else { return };
            let body = body.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let _ = s.read(&mut buf).await;
                let hdr = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = s.write_all(hdr.as_bytes()).await;
                let _ = s.write_all(&body).await;
            });
        }
    });
    port
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scheduled_queue_starts_on_time_and_reports_its_action() {
    // Don't straddle a minute boundary.
    if Local::now().second() >= 55 {
        tokio::time::sleep(Duration::from_secs(6)).await;
    }
    let port = serve(vec![5u8; 300_000]).await;
    let dir = std::env::temp_dir().join(format!("rdm_sched_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let m = Manager::open(&dir);
    let mut events = m.subscribe();
    let id = m.add(
        DownloadRequest { url: format!("http://127.0.0.1:{port}/nightly.bin"), dir: dir.join("out"), ..Default::default() },
        AddOptions { queue: Some(MAIN_QUEUE), ..Default::default() },
    );
    assert_eq!(m.item(id).unwrap().state, ItemState::Queued);
    assert!(!m.is_queue_active(MAIN_QUEUE));

    // Schedule the main queue for "now", run once, hang up when done.
    let now = Local::now();
    let mut s = m.settings();
    let q = s.queues.iter_mut().find(|q| q.id == MAIN_QUEUE).unwrap();
    q.schedule.enabled = true;
    q.schedule.start_at = Some(format!("{:02}:{:02}", now.hour(), now.minute()));
    q.schedule.once = true;
    q.schedule.on_done = OnQueueDone::HangUp;
    m.set_settings(s);

    let mut completed = false;
    let mut done_action = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while tokio::time::Instant::now() < deadline && done_action.is_none() {
        match tokio::time::timeout(Duration::from_millis(500), events.recv()).await {
            Ok(Ok(Event::Completed(i))) if i == id => completed = true,
            Ok(Ok(Event::QueueDone(q, a))) if q == MAIN_QUEUE => done_action = Some(a),
            _ => {}
        }
    }
    assert!(completed, "the scheduled download didn't run; state {:?}", m.item(id).map(|i| i.state));
    assert_eq!(done_action, Some(OnQueueDone::HangUp));
    assert_eq!(m.item(id).unwrap().state, ItemState::Complete);
    let q = m.settings().queues.into_iter().find(|q| q.id == MAIN_QUEUE).unwrap();
    assert!(!q.schedule.enabled, "run-once schedule should switch itself off");
    let _ = std::fs::remove_dir_all(&dir);
}
