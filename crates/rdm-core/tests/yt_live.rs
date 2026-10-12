//! Live check of a YouTube job (only runs when RDM_YT_JOB points at a job JSON file):
//! resolve it, then request the first bytes of the video stream with the engine's client
//! and with curl, one after the other.

use rdm_core::{DownloadRequest, EngineConfig};

#[tokio::test]
async fn engine_and_curl_on_the_same_link() {
    let Ok(path) = std::env::var("RDM_YT_JOB") else { return };
    let job = std::fs::read_to_string(&path).unwrap();
    let id = serde_json::from_str::<serde_json::Value>(&job).unwrap()["video_id"].as_str().unwrap().to_string();
    let req = DownloadRequest { url: format!("https://www.youtube.com/watch?v={id}#rdm-yt"), post_data: Some(job), ..Default::default() };
    let cfg = EngineConfig::default();
    let resolved = rdm_core::youtube::resolve(&req, &cfg).await.expect("resolve");
    let video = resolved.url.split('#').next().unwrap().to_string();
    let single = DownloadRequest { url: video.clone(), ..resolved.clone() };
    let rounds: usize = std::env::var("RDM_YT_ROUNDS").ok().and_then(|s| s.parse().ok()).unwrap_or(2);
    let t0 = std::time::Instant::now();
    for round in 0..rounds {
        if round > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
        eprint!("t={:>3}s ", t0.elapsed().as_secs());
        let engine = rdm_core::probe_info(&single, &cfg).await;
        let curl = std::process::Command::new("curl")
            .args(["-s", "-o", "NUL", "-w", "%{http_code} %{remote_ip}", "-r", "0-65535", "-A", rdm_core::youtube::USER_AGENT, &video])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
        eprintln!("round {round}: engine -> {:?} | curl -> {:?}", engine.as_ref().map(|p| p.total).map_err(|e| e.to_string()), curl);
    }
}
