//! `rdm` — command-line front end.
//!
//! * `rdm get URL`      download directly with a progress bar (resumable).
//! * `rdm add URL`      hand a download to the running RDM app.
//! * `rdm list`         list downloads in the running app.
//! * `rdm daemon`       run the download manager headless (no GUI).

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};
use rdm_core::ipc::{Client, Request, Response};
use rdm_core::settings::data_dir;
use rdm_core::{Download, DownloadRequest, Limiter, Manager, Settings, Status};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "rdm", version, about = "RDM download manager")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Args, Clone, Default)]
struct Opts {
    /// Folder to save into (default: the Downloads folder).
    #[arg(short = 'p', long = "path")]
    dir: Option<PathBuf>,
    /// File name to save as.
    #[arg(short = 'f', long = "file")]
    file: Option<String>,
    /// Maximum connections.
    #[arg(short = 'c', long)]
    connections: Option<usize>,
    /// Speed limit, e.g. 500K or 2M (bytes per second).
    #[arg(short = 'l', long)]
    limit: Option<String>,
    #[arg(long)]
    referer: Option<String>,
    #[arg(long)]
    cookie: Option<String>,
    #[arg(long = "user-agent")]
    user_agent: Option<String>,
    /// Extra header "Name: value" (repeatable).
    #[arg(short = 'H', long = "header")]
    headers: Vec<String>,
    #[arg(long)]
    user: Option<String>,
    #[arg(long)]
    password: Option<String>,
    /// POST body (form downloads).
    #[arg(long)]
    post: Option<String>,
    /// Proxy URL (http://, https://, socks5://) or "none".
    #[arg(long)]
    proxy: Option<String>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Download a URL directly.
    Get {
        url: String,
        #[command(flatten)]
        opts: Opts,
    },
    /// Send a URL to the running RDM app.
    Add {
        url: String,
        /// Start immediately instead of adding to the queue.
        #[arg(long)]
        now: bool,
        #[command(flatten)]
        opts: Opts,
    },
    /// List downloads in the running app.
    List,
    /// Bring the RDM window to the front.
    Show,
    /// Run the manager without a GUI (for servers).
    Daemon,
}

fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    let (num, mult) = match s.chars().last().map(|c| c.to_ascii_uppercase()) {
        Some('K') => (&s[..s.len() - 1], 1024),
        Some('M') => (&s[..s.len() - 1], 1024 * 1024),
        Some('G') => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        _ => (s, 1),
    };
    let v: f64 = num.trim().parse().with_context(|| format!("bad size: {s}"))?;
    Ok((v * mult as f64) as u64)
}

fn build_request(url: String, o: &Opts, settings: &Settings) -> Result<DownloadRequest> {
    let mut headers = vec![];
    for h in &o.headers {
        let (k, v) = h.split_once(':').context("header must be \"Name: value\"")?;
        headers.push((k.trim().to_string(), v.trim().to_string()));
    }
    Ok(DownloadRequest {
        url,
        dir: o.dir.clone().unwrap_or_else(|| settings.download_dir.clone()),
        file_name: o.file.clone(),
        referer: o.referer.clone(),
        cookies: o.cookie.clone(),
        user_agent: o.user_agent.clone(),
        headers,
        post_data: o.post.clone(),
        username: o.user.clone(),
        password: o.password.clone(),
        speed_limit: o.limit.as_deref().map(parse_size).transpose()?,
        max_connections: o.connections,
    })
}

async fn get(url: String, o: Opts) -> Result<()> {
    let settings = Settings::load(&data_dir().join("settings.json"));
    let req = build_request(url.clone(), &o, &settings)?;
    let mut h = DefaultHasher::new();
    (&url, &req.dir, &req.file_name).hash(&mut h);
    let temp = settings.temp_dir.join(format!("cli-{:016x}", h.finish()));
    let mut cfg = settings.engine.clone();
    if let Some(p) = &o.proxy {
        cfg.proxy = Some(p.clone());
    } else if !settings.proxy.mode.is_empty() {
        cfg.proxy = Some(settings.proxy.mode.clone());
    }
    let dl = Download::start(req, cfg, temp, Arc::new(Limiter::new(settings.speed_limit)));
    let handle = dl.handle();
    let mut progress = dl.progress.clone();

    let bar = ProgressBar::new(0);
    bar.set_style(
        ProgressStyle::with_template("{msg}\n[{bar:40.cyan/blue}] {bytes}/{total_bytes} {binary_bytes_per_sec} eta {eta} ({prefix} conn)")
            .unwrap()
            .progress_chars("=> "),
    );
    let ui = tokio::spawn(async move {
        loop {
            {
                let p = progress.borrow().clone();
                if let Some(t) = p.total {
                    bar.set_length(t);
                }
                bar.set_position(p.downloaded);
                bar.set_prefix(p.connections.to_string());
                bar.set_message(p.file_name.clone());
                if matches!(p.status, Status::Complete | Status::Failed(_) | Status::Paused) {
                    bar.finish_and_clear();
                    break;
                }
            }
            if progress.changed().await.is_err() {
                break;
            }
        }
    });
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\npausing; run the same command again to resume");
            handle.pause();
        }
    });
    let res = dl.wait().await;
    let _ = ui.await;
    match res {
        Ok(Some(path)) => println!("saved {}", path.display()),
        Ok(None) => println!("paused"),
        Err(e) => bail!("download failed: {e}"),
    }
    Ok(())
}

async fn client() -> Result<Client> {
    Client::connect(&data_dir()).await.context("RDM is not running (start the app or `rdm daemon`)")
}

async fn daemon() -> Result<()> {
    let dir = data_dir();
    let m = Manager::open(&dir);
    rdm_core::ipc::serve(m.clone(), None).await?;
    m.start_queue(rdm_core::settings::MAIN_QUEUE);
    println!("RDM daemon running (data: {}). Ctrl+C to stop.", dir.display());
    tokio::signal::ctrl_c().await?;
    m.shutdown().await;
    Ok(())
}

fn human(b: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < units.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{b} B") } else { format!("{v:.1} {}", units[i]) }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).with_writer(std::io::stderr).init();
    match Cli::parse().cmd {
        Cmd::Get { url, opts } => get(url, opts).await,
        Cmd::Add { url, now, opts } => {
            let settings = Settings::load(&data_dir().join("settings.json"));
            let mut req = build_request(url, &opts, &settings)?;
            if opts.dir.is_none() {
                req.dir = PathBuf::new(); // let the app choose the category folder
            }
            match client().await?.call(Request::Add { request: req, start_now: now, interactive: false }).await? {
                Response::Added { id } => println!("added{}", id.map(|i| format!(" (#{i})")).unwrap_or_default()),
                Response::Error { message } => bail!(message),
                other => println!("{other:?}"),
            }
            Ok(())
        }
        Cmd::List => {
            if let Response::Items { items } = client().await?.call(Request::List).await? {
                for i in items {
                    let size = i.total.map(human).unwrap_or_else(|| "?".into());
                    println!("#{:<4} {:<12} {:>10} / {:<10} {}", i.id, i.state, human(i.downloaded), size, i.file_name);
                }
            }
            Ok(())
        }
        Cmd::Show => {
            client().await?.call(Request::Show).await?;
            Ok(())
        }
        Cmd::Daemon => daemon().await,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn sizes() {
        assert_eq!(super::parse_size("500K").unwrap(), 512_000);
        assert_eq!(super::parse_size("2M").unwrap(), 2 * 1024 * 1024);
        assert_eq!(super::parse_size("100").unwrap(), 100);
    }
}
