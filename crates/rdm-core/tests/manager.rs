//! The manager is driven from the UI thread, which is outside the async runtime.

use rdm_core::{AddOptions, DownloadRequest, Manager};

#[test]
fn start_from_a_thread_outside_the_runtime() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let dir = std::env::temp_dir().join(format!("rdm_mgr_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let manager = {
        let _g = rt.enter();
        Manager::open(&dir)
    };
    // Like the UI: a plain thread adds and starts a download.
    let m = manager.clone();
    let out = dir.join("out");
    std::thread::spawn(move || {
        let id = m.add(
            DownloadRequest { url: "http://127.0.0.1:9/never.bin".into(), dir: out, ..Default::default() },
            AddOptions { start_now: true, ..Default::default() },
        );
        m.start(id);
    })
    .join()
    .expect("starting a download outside the runtime must not panic");
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert_eq!(manager.items().len(), 1);
    drop(rt);
    let _ = std::fs::remove_dir_all(&dir);
}
