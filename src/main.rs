use std::{path::PathBuf, sync::Arc};

mod compose;
mod discord;
mod files;
mod http;
mod render;
mod requests;
mod session;
mod voice;
mod worker;

const DEFAULT_PORT: u16 = 38445;

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tokio::task::LocalSet::new().run_until(run()).await
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    // Capture dimensions must be physical pixels, including on scaled Windows displays.
    #[cfg(target_os = "windows")]
    unsafe {
        use windows_sys::Win32::UI::HiDpi::{
            SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        };
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let started = std::time::Instant::now();
    let mut port = DEFAULT_PORT;
    let mut data = None;
    let mut account = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--data-dir" => {
                data = Some(PathBuf::from(args.next().ok_or("--data-dir needs a path")?))
            }
            "--account" => account = Some(args.next().ok_or("--account needs an ID")?),
            "--port" => port = args.next().ok_or("--port needs a number")?.parse()?,
            "--help" => {
                println!("suncord-browser [--port PORT] [--data-dir PATH] [--account ID]\nUses port {DEFAULT_PORT} by default. Use --port 0 for a random port.");
                return Ok(());
            }
            _ => return Err(format!("Unknown argument: {arg}").into()),
        }
    }
    let cache = discord::cache::CacheStore::default();
    let data = data.unwrap_or_else(|| {
        #[cfg(target_os = "windows")]
        let root = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
        #[cfg(not(target_os = "windows"))]
        let root = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
            });
        root.unwrap_or_else(|| PathBuf::from("."))
            .join("com.suncord.desktop")
    });
    let uploads = Arc::new(files::Uploads::default());
    let (commands, session) = worker::start(cache.clone(), data.clone(), account, uploads.clone())?;
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
        .await
        .map_err(|error| format!("Could not listen on port {port}: {error}. Is Suncord already running? Use --port to choose another."))?;
    let port = listener.local_addr()?.port();
    let secret = http::saved_secret(&data)?;
    let app = Arc::new(http::App {
        data,
        cache,
        session,
        commands,
        hosts: [format!("127.0.0.1:{port}"), format!("localhost:{port}")],
        secret: secret.clone(),
        cookie: format!("suncord_{port}"),
        uploads,
    });
    eprintln!(
        "ready in {:.1} ms",
        started.elapsed().as_secs_f64() * 1000.0
    );
    println!("http://localhost:{port}/#{secret}");
    axum::serve(listener, http::router(app.clone()))
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            let (reply, done) = tokio::sync::oneshot::channel();
            let guard = app.session.gate.clone().write_owned().await;
            let _ = app
                .commands
                .send(worker::Request::Shutdown(reply, guard))
                .await;
            let _ = done.await;
        })
        .await?;
    Ok(())
}

/// Replaces a file atomically. The new file is private to the user.
fn write_file(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("missing parent directory"))?;
    std::fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    std::io::Write::write_all(&mut file, bytes)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}
