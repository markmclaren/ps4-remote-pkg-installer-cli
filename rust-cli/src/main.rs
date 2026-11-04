use anyhow::{anyhow, bail, Context, Result};
use axum::{
    extract::{Path as UrlPath, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use clap::{Parser, Subcommand};
use regex::Regex;
use reqwest::blocking::Client;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs::File;
use std::hash::{BuildHasher, Hasher};
use std::io::Read;
use std::net::{IpAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, OnceLock,
};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tower::ServiceExt;
use tower_http::services::ServeFile;

#[derive(Parser)]
#[command(
    name = "ps4-remote-pkg-installer-cli",
    version,
    about = "Install and manage PS4 Remote PKG Installer tasks"
)]
struct Cli {
    /// PS4 hostname or IP address (or set PS4_CONSOLE)
    #[arg(long, global = true, env = "PS4_CONSOLE")]
    console: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check whether Remote PKG Installer is responding
    Status,
    /// Check whether a title is already installed
    Exists { title_id: String },
    /// Install local PKG files and serve them until the task finishes
    Install {
        #[arg(required = true)]
        files: Vec<PathBuf>,
        /// IP address on this PC that the console can reach
        #[arg(long)]
        advertise_ip: Option<IpAddr>,
        /// Local HTTP port used to serve the PKG files
        #[arg(long, default_value_t = 8080)]
        file_server_port: u16,
        /// Seconds between task progress checks
        #[arg(long, default_value_t = 2)]
        poll_seconds: u64,
        /// Show exact byte counts instead of KiB/MiB/GiB
        #[arg(long)]
        raw: bool,
    },
    /// Install a PKG from an HTTP(S) URL
    InstallUrl { url: String },
    /// Install packages listed by a manifest JSON URL
    InstallManifest { url: String },
    /// Show progress for tasks started by this CLI
    Tasks {
        /// Show exact byte counts instead of KiB/MiB/GiB
        #[arg(long)]
        raw: bool,
    },
    /// Pause a task
    Pause { task_id: u32 },
    /// Resume a task
    Resume { task_id: u32 },
    /// Stop a task
    Stop { task_id: u32 },
    /// Unregister a task
    Remove { task_id: u32 },
}

#[derive(Deserialize)]
struct ApiResponse {
    status: Option<String>,
    error: Option<Value>,
    error_code: Option<String>,
    task_id: Option<u32>,
    exists: Option<Value>,
}

impl ApiResponse {
    // The console sends "exists" as the string "true"/"false".
    fn exists(&self) -> Option<bool> {
        match self.exists.as_ref()? {
            Value::Bool(b) => Some(*b),
            Value::String(s) => Some(s == "true"),
            _ => None,
        }
    }
}

#[derive(Clone)]
struct FileServerState {
    files: Arc<HashMap<String, PathBuf>>,
    last_activity_ms: Arc<AtomicU64>,
}

fn epoch_millis_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ServeFile handles Range, 206/416 and conditional requests.
async fn serve_pkg(
    State(state): State<FileServerState>,
    UrlPath(key): UrlPath<String>,
    request: Request,
) -> Response {
    state
        .last_activity_ms
        .store(epoch_millis_now(), Ordering::Relaxed);
    match state.files.get(&key) {
        Some(path) => match ServeFile::new(path)
            .with_buf_chunk_size(2 << 20)
            .oneshot(request)
            .await
        {
            Ok(response) => {
                state
                    .last_activity_ms
                    .store(epoch_millis_now(), Ordering::Relaxed);
                response.into_response()
            }
            Err(never) => match never {},
        },
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// Unguessable URL token from the OS-seeded std hasher.
fn random_token() -> String {
    let state = std::collections::hash_map::RandomState::new();
    let mut a = state.build_hasher();
    a.write_u8(1);
    let mut b = state.build_hasher();
    b.write_u8(2);
    format!("{:016x}{:016x}", a.finish(), b.finish())
}

#[derive(Deserialize, Debug, Clone)]
#[allow(dead_code)]
struct TaskProgress {
    bits: Option<Value>,
    error: Option<Value>,
    transferred: Option<Value>,
    length: Option<Value>,
    transferred_total: Option<Value>,
    length_total: Option<Value>,
    num_index: Option<Value>,
    num_total: Option<Value>,
    rest_sec: Option<Value>,
    rest_sec_total: Option<Value>,
    preparing_percent: Option<Value>,
    local_copy_percent: Option<Value>,
}

struct ApiClient {
    client: Client,
    base_url: String,
}

impl ApiClient {
    fn new(console: &str) -> Result<Self> {
        let host =
            if console.starts_with('[') || console.parse::<IpAddr>().is_ok_and(|ip| ip.is_ipv4()) {
                console.to_string()
            } else if console.parse::<IpAddr>().is_ok() {
                format!("[{console}]")
            } else {
                console.to_string()
            };
        Ok(Self {
            client: Client::builder()
                .timeout(Duration::from_secs(15))
                .pool_max_idle_per_host(0)
                .build()
                .context("creating HTTP client")?,
            base_url: format!("http://{host}:12800/api"),
        })
    }

    fn post(&self, route: &str, body: Value) -> Result<Value> {
        static HEX_VALUE_RE: OnceLock<Regex> = OnceLock::new();
        let hex_value = HEX_VALUE_RE
            .get_or_init(|| Regex::new(r"(:\s*)(0[xX][0-9a-fA-F]+)\b").expect("valid regex"));

        // Do not retry non-idempotent /install (could register duplicate tasks)
        // or /get_task_progress (which has its own outer polling retry loop).
        let (max_attempts, timeout) = match route {
            "/install" => (1, Duration::from_secs(60)),
            "/get_task_progress" => (1, Duration::from_secs(10)),
            _ => (3, Duration::from_secs(15)),
        };

        let mut last_err = None;
        for attempt in 1..=max_attempts {
            let res = self
                .client
                .post(format!("{}{route}", self.base_url))
                .timeout(timeout)
                .header(reqwest::header::CONNECTION, "close")
                .json(&body)
                .send();

            match res {
                Ok(resp) => {
                    match resp.text() {
                        Ok(text) => {
                            let normalized = hex_value.replace_all(&text, "$1\"$2\"");
                            return serde_json::from_str(&normalized)
                                .with_context(|| format!("invalid JSON from {route}: {text}"));
                        }
                        Err(e) => {
                            last_err = Some(anyhow::Error::from(e).context(format!("reading response from {route}")));
                        }
                    }
                }
                Err(e) => {
                    last_err = Some(anyhow::Error::from(e).context(format!("request to {route} failed")));
                }
            }

            if attempt < max_attempts {
                thread::sleep(Duration::from_millis(300 * attempt));
            }
        }

        Err(last_err.unwrap_or_else(|| anyhow!("request to {route} failed")))
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let console = cli.console.as_deref().context("--console or the PS4_CONSOLE environment variable is required")?;
    let api = ApiClient::new(console)?;

    match cli.command {
        Command::Status => {
            let response = api.post("/is_exists", json!({ "title_id": "" }))?;
            let response: ApiResponse = serde_json::from_value(response)?;
            ensure_success(&response)?;
            println!(
                "Remote PKG Installer is {}",
                if response.exists() == Some(false) {
                    "running"
                } else {
                    "not responding"
                }
            );
        }
        Command::Exists { title_id } => {
            let response = api.post("/is_exists", json!({ "title_id": title_id }))?;
            let response: ApiResponse = serde_json::from_value(response)?;
            ensure_success(&response)?;
            println!("{}", response.exists().unwrap_or(false));
        }
        Command::Install {
            files,
            advertise_ip,
            file_server_port,
            poll_seconds,
            raw,
        } => {
            install_files(
                &api,
                files,
                advertise_ip,
                file_server_port,
                poll_seconds,
                !raw,
            )?;
        }
        Command::InstallUrl { url } => {
            let response = api.post("/install", json!({ "type": "direct", "packages": [url] }))?;
            report_install(response)?;
        }
        Command::InstallManifest { url } => {
            let response = api.post("/install", json!({ "type": "ref_pkg_url", "url": url }))?;
            report_install(response)?;
        }
        Command::Tasks { raw } => show_tasks(&api, !raw)?,
        Command::Pause { task_id } => modify_task(&api, task_id, "pause")?,
        Command::Resume { task_id } => modify_task(&api, task_id, "resume")?,
        Command::Stop { task_id } => modify_task(&api, task_id, "stop")?,
        Command::Remove { task_id } => {
            modify_task(&api, task_id, "unregister")?;
            let mut ids = load_task_ids()?;
            ids.remove(&task_id);
            save_task_ids(&ids)?;
        }
    }
    Ok(())
}

fn install_files(
    api: &ApiClient,
    files: Vec<PathBuf>,
    advertise_ip: Option<IpAddr>,
    port: u16,
    poll_seconds: u64,
    human: bool,
) -> Result<()> {
    let files = files
        .into_iter()
        .map(|path| {
            let path = path
                .canonicalize()
                .with_context(|| format!("file not found: {}", path.display()))?;
            if !path.is_file() {
                bail!("not a file: {}", path.display());
            }
            Ok(path)
        })
        .collect::<Result<Vec<_>>>()?;
    let ip = match advertise_ip {
        Some(ip) => ip,
        None => route_ip(&api.base_url)?,
    };

    // Pre-flight check: Ensure PS4 is responsive before launching server
    println!("Checking PS4 Remote PKG Installer status...");
    let mut console_ready = false;
    for attempt in 1..=5 {
        if api.post("/is_exists", json!({ "title_id": "" })).is_ok() {
            console_ready = true;
            break;
        }
        if attempt < 5 {
            println!("Console busy or not responding; retrying... (attempt {attempt}/5)");
            thread::sleep(Duration::from_secs(2));
        }
    }
    if !console_ready {
        bail!("PS4 Remote PKG Installer is not responding. Please make sure the app is open on your PS4.");
    }

    let tokens: Vec<String> = files.iter().map(|_| random_token()).collect();
    let files_by_key: Arc<HashMap<String, PathBuf>> = Arc::new(
        tokens
            .iter()
            .cloned()
            .zip(files.iter().cloned())
            .collect(),
    );
    let last_activity_ms = Arc::new(AtomicU64::new(epoch_millis_now()));
    let server_state = FileServerState {
        files: files_by_key,
        last_activity_ms: Arc::clone(&last_activity_ms),
    };

    // Bind socket and retry if port was recently used
    let socket = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )?;
    #[cfg(not(windows))]
    socket.set_reuse_address(true)?;
    let _ = socket.set_nodelay(true);
    let _ = socket.set_send_buffer_size(8 << 20);

    let addr: std::net::SocketAddr = ([0, 0, 0, 0], port).into();
    for attempt in 1..=5 {
        match socket.bind(&addr.into()) {
            Ok(()) => break,
            Err(e) => {
                if attempt == 5 {
                    return Err(anyhow!("could not bind local file server on port {port}: {e}"));
                }
                println!("Port {port} in cooldown; retrying in 1s... ({attempt}/5)");
                thread::sleep(Duration::from_secs(1));
            }
        }
    }

    socket.listen(1024)?;
    let listener: std::net::TcpListener = socket.into();
    listener.set_nonblocking(true)?;
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server_thread = thread::spawn(move || {
        let Ok(runtime) = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        runtime.block_on(async move {
            let Ok(listener) = tokio::net::TcpListener::from_std(listener) else {
                return;
            };
            let app = Router::new()
                .route("/pkg/:key", get(serve_pkg))
                .with_state(server_state);
            tokio::select! {
                _ = axum::serve(listener, app) => {}
                _ = shutdown_rx => {}
            }
        });
        // Drop open keep-alive connections instead of waiting on them.
        runtime.shutdown_background();
    });

    let urls: Vec<String> = tokens
        .iter()
        .map(|token| format!("http://{ip}:{port}/pkg/{token}"))
        .collect();
    let local_files: Vec<String> = files
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    let response = api.post(
        "/install",
        json!({ "type": "direct", "packages": urls, "local_files": local_files }),
    );

    let file_names: Vec<String> = files
        .iter()
        .map(|path| {
            path.file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string())
        })
        .collect();

    let outcome = (|| -> Result<()> {
        let response = response?;
        let install: ApiResponse = serde_json::from_value(response)?;
        ensure_success(&install)?;
        let task_id = install
            .task_id
            .context("install response did not contain a task_id")?;
        let mut ids = load_task_ids()?;
        ids.insert(task_id);
        save_task_ids(&ids)?;
        println!(
            "Started task {task_id}; serving {} file(s) from {ip}:{port}",
            files.len()
        );
        wait_for_task(
            api,
            task_id,
            &file_names,
            poll_seconds.max(1),
            human,
            &last_activity_ms,
        )
    })();

    let _ = shutdown_tx.send(());
    let _ = server_thread.join();
    outcome
}

fn route_ip(api_base_url: &str) -> Result<IpAddr> {
    let host = api_base_url
        .strip_prefix("http://")
        .and_then(|value| value.rsplit_once(':').map(|(host, _)| host))
        .unwrap_or_default()
        .trim_start_matches('[')
        .trim_end_matches(']');
    let socket = UdpSocket::bind("0.0.0.0:0").context("selecting network interface")?;
    let address = if host.parse::<IpAddr>().is_ok_and(|ip| ip.is_ipv6()) {
        format!("[{host}]:12800")
    } else {
        format!("{host}:12800")
    };
    socket
        .connect(address)
        .context("finding the network interface used to reach the console")?;
    Ok(socket.local_addr()?.ip())
}

fn parse_number(text: &str) -> Option<u64> {
    text.strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .and_then(|hex| u64::from_str_radix(hex, 16).ok())
        .or_else(|| text.parse::<u64>().ok())
        .or_else(|| text.parse::<i64>().ok().map(|v| (v as u32) as u64))
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn value_to_u64(val: Option<&Value>) -> Option<u64> {
    match val {
        Some(Value::Number(n)) => n
            .as_u64()
            .or_else(|| n.as_i64().map(|v| (v as u32) as u64)),
        Some(Value::String(s)) => parse_number(s),
        _ => None,
    }
}

fn format_seconds(secs: u64) -> String {
    if secs >= 3600 {
        format!("{:02}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
    } else {
        format!("{:02}:{:02}", secs / 60, secs % 60)
    }
}

fn render_bar(ratio: f64, width: usize) -> String {
    let clamped = ratio.clamp(0.0, 1.0);
    let filled = (clamped * width as f64).round() as usize;
    let empty = width.saturating_sub(filled);
    format!("[{}{}]", "=".repeat(filled), " ".repeat(empty))
}

fn wait_for_console_ready(api: &ApiClient, cooldown_secs: u64) {
    use std::io::Write;
    for i in (1..=cooldown_secs).rev() {
        print!("\rWaiting for PS4 to finalize installation and settle ({i}s)... ");
        std::io::stdout().flush().ok();
        thread::sleep(Duration::from_secs(1));
    }

    // Ping the console to confirm readiness
    let mut ready = false;
    for _ in 0..5 {
        if api.post("/is_exists", json!({ "title_id": "" })).is_ok() {
            ready = true;
            break;
        }
        thread::sleep(Duration::from_millis(500));
    }

    if ready {
        println!("\r✓ PS4 is ready for the next package!                                  ");
    } else {
        println!("\rPS4 settling period complete.                                         ");
    }
}

fn wait_for_task(
    api: &ApiClient,
    task_id: u32,
    file_names: &[String],
    poll_seconds: u64,
    human: bool,
    last_activity_ms: &AtomicU64,
) -> Result<()> {
    let start_time = std::time::Instant::now();
    let mut last_sample_time = std::time::Instant::now();
    let mut last_bytes: Option<u64> = None;
    let mut smoothed_speed: Option<f64> = None;
    let mut last_line_len: usize = 0;
    let mut consecutive_poll_errors: u32 = 0;
    const MAX_POLL_RETRIES: u32 = 60; // Allow 2+ minutes of PS4 disk saturation without terminating

    let total_files = file_names.len();
    if total_files == 1 {
        println!("Target file: {}", file_names[0]);
    } else if total_files > 1 {
        println!("Target files ({} parts): {}", total_files, file_names.join(", "));
    }

    loop {
        thread::sleep(Duration::from_secs(poll_seconds));
        let now = std::time::Instant::now();

        let value = match api.post("/get_task_progress", json!({ "task_id": task_id })) {
            Ok(v) => {
                consecutive_poll_errors = 0;
                v
            }
            Err(e) => {
                let last_act = last_activity_ms.load(Ordering::Relaxed);
                let active_recently = epoch_millis_now().saturating_sub(last_act) < 30_000;
                if active_recently && consecutive_poll_errors > 0 {
                    // Keep the file server alive while PS4 BGFT is actively streaming PKG data
                    consecutive_poll_errors = 1;
                } else {
                    consecutive_poll_errors += 1;
                }
                if consecutive_poll_errors >= MAX_POLL_RETRIES {
                    println!();
                    eprintln!("✕ Lost connection to PS4 console after {MAX_POLL_RETRIES} attempts: {e}");
                    return Err(e);
                }
                let retry_notice = format!("[PS4 busy / reconnecting... attempt {consecutive_poll_errors}/{MAX_POLL_RETRIES}]");
                print!("\r{:<80}", retry_notice);
                use std::io::Write;
                std::io::stdout().flush().ok();
                continue;
            }
        };

        let response: ApiResponse = match serde_json::from_value(value.clone()) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if let Err(e) = ensure_success(&response) {
            consecutive_poll_errors += 1;
            if consecutive_poll_errors >= MAX_POLL_RETRIES {
                return Err(e);
            }
            continue;
        }

        let progress: TaskProgress = match serde_json::from_value(value) {
            Ok(p) => p,
            Err(_) => continue,
        };

        // Check for PS4 BGFT system error code
        if let Some(err_code) = value_to_u64(progress.error.as_ref()) {
            if err_code != 0 {
                println!();
                let msg = match err_code {
                    0x80990088 => "Application or package is already installed on the console.",
                    0x80990004 | 0x8099000c | 0x80990011 => "Insufficient free storage space on PS4 hard drive.",
                    0x80990014 => "Network connection lost by PS4 background transfer service.",
                    _ => "Internal BGFT installation error.",
                };
                eprintln!("✕ PS4 reported error 0x{err_code:08X}: {msg}");
                let _ = api.post("/unregister_task", json!({ "task_id": task_id }));
                bail!("Task {task_id} failed on PS4: 0x{err_code:08X} ({msg})");
            }
        }

        let mut raw_status = task_status(progress.bits.as_ref());
        let transferred = value_to_u64(progress.transferred_total.as_ref())
            .or_else(|| value_to_u64(progress.transferred.as_ref()))
            .unwrap_or(0);
        let total_length = value_to_u64(progress.length_total.as_ref())
            .or_else(|| value_to_u64(progress.length.as_ref()))
            .unwrap_or(0);
        let local_copy_pct = value_to_u64(progress.local_copy_percent.as_ref()).unwrap_or(0);

        if raw_status != "Installed"
            && raw_status != "Stopped"
            && total_length > 0
            && transferred >= total_length
            && local_copy_pct >= 100
        {
            raw_status = "Installed";
        }

        let piece_idx = value_to_u64(progress.num_index.as_ref()).map(|v| v as usize);
        let piece_total = value_to_u64(progress.num_total.as_ref())
            .map(|v| v as usize)
            .unwrap_or(total_files);

        // Speed calculation
        let dt = now.duration_since(last_sample_time).as_secs_f64();
        if dt >= 0.5 {
            if let Some(prev) = last_bytes {
                if transferred >= prev {
                    let instant_speed = (transferred - prev) as f64 / dt;
                    smoothed_speed = Some(match smoothed_speed {
                        Some(prev_spd) => prev_spd * 0.7 + instant_speed * 0.3,
                        None => instant_speed,
                    });
                }
            }
            last_bytes = Some(transferred);
            last_sample_time = now;
        }

        // Percentage & bar
        let ratio = if total_length > 0 {
            (transferred as f64 / total_length as f64).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let percent = ratio * 100.0;
        let bar = render_bar(ratio, 25);

        // Formatted bytes
        let bytes_str = if human {
            format!("{}/{}", human_bytes(transferred), human_bytes(total_length))
        } else {
            format!("{}/{} B", transferred, total_length)
        };

        // Determine user-friendly status
        let status = if (raw_status == "Downloading" || raw_status == "Unknown")
            && total_length > 0
            && transferred >= total_length
        {
            "Installing"
        } else if raw_status == "Unknown" && total_length > 0 && transferred < total_length {
            "Downloading"
        } else {
            raw_status
        };

        // Speed string
        let speed_str = match smoothed_speed {
            Some(spd) if spd > 1024.0 && status == "Downloading" => format!(" | {}/s", human_bytes(spd as u64)),
            _ => String::new(),
        };

        // ETA string
        let eta_secs = value_to_u64(progress.rest_sec_total.as_ref())
            .or_else(|| value_to_u64(progress.rest_sec.as_ref()))
            .or_else(|| {
                smoothed_speed.and_then(|spd| {
                    if spd > 1024.0 && total_length > transferred {
                        Some(((total_length - transferred) as f64 / spd) as u64)
                    } else {
                        None
                    }
                })
            });
        let eta_str = match eta_secs {
            Some(s) if s > 0 && status == "Downloading" => format!(" | ETA: {}", format_seconds(s)),
            _ => String::new(),
        };

        // Part indicator if multi-part
        let part_str = if piece_total > 1 {
            let idx = piece_idx.unwrap_or(0) + 1;
            format!(" [Part {}/{}]", idx, piece_total)
        } else {
            String::new()
        };

        let line = if status == "Installing" {
            format!("{bar} 100.0% | {bytes_str} [Installing to PS4 hard drive...]")
        } else {
            format!("{bar} {percent:5.1}% | {bytes_str}{speed_str}{eta_str}{part_str} [{status}]")
        };

        let padding = if line.len() < last_line_len {
            " ".repeat(last_line_len - line.len())
        } else {
            String::new()
        };
        print!("\r{line}{padding}");
        use std::io::Write;
        std::io::stdout().flush().ok();
        last_line_len = line.len();

        if raw_status == "Installed" {
            println!();
            let elapsed = start_time.elapsed().as_secs();
            println!(
                "✓ Task {task_id} completed successfully in {}!",
                format_seconds(elapsed)
            );

            // Post-install Readiness / Cooldown:
            // Allow the PS4 OS (SceShellCore) to finalize installation before unregistering the BGFT task
            wait_for_console_ready(api, 4);

            // Clean up: Unregister completed task from PS4 BGFT so it doesn't exhaust BGFT memory
            let _ = api.post("/unregister_task", json!({ "task_id": task_id }));

            // Clean up from local tracked state
            if let Ok(mut ids) = load_task_ids() {
                ids.remove(&task_id);
                let _ = save_task_ids(&ids);
            }
            break;
        } else if raw_status == "Stopped" {
            println!();
            eprintln!("✕ Task {task_id} was stopped or aborted.");
            let _ = api.post("/unregister_task", json!({ "task_id": task_id }));
            break;
        }
    }
    Ok(())
}

fn task_status(bits: Option<&Value>) -> &'static str {
    let Some(bits) = bits else { return "Unknown" };
    let text = bits
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| bits.to_string());
    let numeric = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .and_then(|hex| u64::from_str_radix(hex, 16).ok())
        .or_else(|| text.parse::<u64>().ok());
    // Mask to the low byte because libSceBgft can leave non-zero flags/bytes
    // in the upper bits of SceBgftTaskProgress.bits (e.g. 0x3633358a / 0x36333593).
    match numeric.map(|v| v & 0xff) {
        Some(0x8a) => "Downloading",
        Some(0x8e) => "Installing",
        Some(0x93) => "Installed",
        Some(0xa8) => "Paused",
        Some(0x88) => "Stopped",
        _ => "Unknown",
    }
}

fn show_tasks(api: &ApiClient, human: bool) -> Result<()> {
    let ids = load_task_ids()?;
    if ids.is_empty() {
        println!("No tracked tasks. Start an install to add one.");
        return Ok(());
    }
    println!("{:<8} {:<14} {:<26} {:<10} {:<10}", "TASK ID", "STATUS", "PROGRESS", "PERCENT", "ETA");
    println!("{}", "-".repeat(72));
    for id in ids {
        let value = api.post("/get_task_progress", json!({ "task_id": id }))?;
        let response: ApiResponse = serde_json::from_value(value.clone())?;
        ensure_success(&response)?;
        let task: TaskProgress = serde_json::from_value(value)?;
        let status = task_status(task.bits.as_ref());
        let transferred = value_to_u64(task.transferred_total.as_ref())
            .or_else(|| value_to_u64(task.transferred.as_ref()))
            .unwrap_or(0);
        let length = value_to_u64(task.length_total.as_ref())
            .or_else(|| value_to_u64(task.length.as_ref()))
            .unwrap_or(0);
        let percent_str = if length > 0 {
            format!("{:5.1}%", (transferred as f64 / length as f64 * 100.0).clamp(0.0, 100.0))
        } else {
            "  N/A".to_string()
        };
        let eta_secs = value_to_u64(task.rest_sec_total.as_ref())
            .or_else(|| value_to_u64(task.rest_sec.as_ref()));
        let eta_str = match eta_secs {
            Some(s) if s > 0 => format_seconds(s),
            _ => "-".to_string(),
        };
        let prog_str = if human {
            format!("{}/{}", human_bytes(transferred), human_bytes(length))
        } else {
            format!("{}/{} B", transferred, length)
        };
        println!("{:<8} {:<14} {:<26} {:<10} {:<10}", id, status, prog_str, percent_str, eta_str);
    }
    Ok(())
}

fn modify_task(api: &ApiClient, task_id: u32, action: &str) -> Result<()> {
    let value = api.post(&format!("/{action}_task"), json!({ "task_id": task_id }))?;
    let response: ApiResponse = serde_json::from_value(value)?;
    ensure_success(&response)?;
    println!("Task {task_id}: {action} requested");
    Ok(())
}

fn report_install(value: Value) -> Result<()> {
    let response: ApiResponse = serde_json::from_value(value)?;
    ensure_success(&response)?;
    if let Some(task_id) = response.task_id {
        let mut ids = load_task_ids()?;
        ids.insert(task_id);
        save_task_ids(&ids)?;
        println!("Started task {task_id}");
    }
    Ok(())
}

fn ensure_success(response: &ApiResponse) -> Result<()> {
    if response.status.as_deref() == Some("success") {
        return Ok(());
    }
    bail!(
        "console request failed (error: {}, error_code: {})",
        response
            .error
            .as_ref()
            .map(Value::to_string)
            .unwrap_or_else(|| "unknown".into()),
        response.error_code.as_deref().unwrap_or("unknown")
    )
}

fn task_state_path() -> Result<PathBuf> {
    let app_data = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE").map(|path| PathBuf::from(path).join("AppData/Roaming"))
        })
        .context("could not find the Windows application data directory")?;
    Ok(app_data
        .join("ps4-remote-pkg-installer-cli")
        .join("tasks.json"))
}

fn load_task_ids() -> Result<std::collections::BTreeSet<u32>> {
    let path = task_state_path()?;
    if !path.exists() {
        return Ok(Default::default());
    }
    let mut contents = String::new();
    File::open(path)?.read_to_string(&mut contents)?;
    Ok(serde_json::from_str(&contents).context("reading saved task IDs")?)
}

fn save_task_ids(ids: &std::collections::BTreeSet<u32>) -> Result<()> {
    let path = task_state_path()?;
    std::fs::create_dir_all(path.parent().context("invalid task state path")?)?;
    std::fs::write(path, serde_json::to_vec_pretty(ids)?)?;
    Ok(())
}
