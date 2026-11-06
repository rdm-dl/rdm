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

#[test]
fn rename_finished_and_unfinished() {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for s in listener.incoming() {
            let Ok(mut s) = s else { return };
            let mut buf = [0u8; 2048];
            let _ = s.read(&mut buf);
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello");
        }
    });
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let dir = std::env::temp_dir().join(format!("rdm_rename_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let manager = {
        let _g = rt.enter();
        Manager::open(&dir)
    };
    let out = dir.join("out");

    // Not started yet: the name is used when it completes.
    let later = manager.add(DownloadRequest { url: format!("http://127.0.0.1:{port}/a.bin"), dir: out.clone(), ..Default::default() }, AddOptions::default());
    manager.rename(later, "chosen.bin").unwrap();
    assert_eq!(manager.item(later).unwrap().file_name, "chosen.bin");
    assert!(manager.rename(later, "   ").is_err());
    manager.start(later);
    let wait = |id| {
        for _ in 0..200 {
            if manager.item(id).unwrap().state == rdm_core::ItemState::Complete {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        panic!("download did not finish: {:?}", manager.item(id).unwrap().state);
    };
    wait(later);
    assert_eq!(std::fs::read(out.join("chosen.bin")).unwrap(), b"hello");

    // Finished: the file itself moves, and an existing name is refused.
    let other = manager.add(DownloadRequest { url: format!("http://127.0.0.1:{port}/b.bin"), dir: out.clone(), ..Default::default() }, AddOptions { start_now: true, ..Default::default() });
    manager.start(other);
    wait(other);
    assert!(manager.rename(other, "chosen.bin").unwrap_err().contains("already exists"));
    manager.rename(other, "final.bin").unwrap();
    assert!(out.join("final.bin").exists() && !out.join("b.bin").exists());
    assert_eq!(manager.item(other).unwrap().final_path.unwrap(), out.join("final.bin"));
    manager.rename(other, "FINAL.bin").unwrap();
    drop(rt);
    let _ = std::fs::remove_dir_all(&dir);
}
