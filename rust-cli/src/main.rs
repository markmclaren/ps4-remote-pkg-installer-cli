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
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tower::ServiceExt;
use tower_http::services::ServeFile;

#[derive(Parser)]
#[command(
    name = "ps4-remote-pkg-installer-cli",
    version,
    about = "Install and manage PS4 Remote PKG Installer tasks"
)]
struct Cli {
    /// PS4 hostname or IP address
    #[arg(long, global = true)]
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
        #[arg(long, default_value_t = 3)]
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

type SharedFiles = Arc<HashMap<String, PathBuf>>;

// ServeFile handles Range, 206/416 and conditional requests.
async fn serve_pkg(
    State(files): State<SharedFiles>,
    UrlPath(key): UrlPath<String>,
    request: Request,
) -> Response {
    match files.get(&key) {
        Some(path) => match ServeFile::new(path)
            .with_buf_chunk_size(1 << 20)
            .oneshot(request)
            .await
        {
            Ok(response) => response.into_response(),
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

#[derive(Deserialize)]
struct TaskProgress {
    bits: Option<Value>,
    transferred: Option<String>,
    length: Option<String>,
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
                .timeout(Duration::from_secs(10))
                .build()
                .context("creating HTTP client")?,
            base_url: format!("http://{host}:12800/api"),
        })
    }

    fn post(&self, route: &str, body: Value) -> Result<Value> {
        let text = self
            .client
            .post(format!("{}{route}", self.base_url))
            .json(&body)
            .send()
            .with_context(|| format!("request to {route} failed"))?
            .text()
            .with_context(|| format!("reading response from {route}"))?;

        let hex_value = Regex::new(r"(:\s*)(0[xX][0-9a-fA-F]+)\b").expect("valid regex");
        let normalized = hex_value.replace_all(&text, "$1\"$2\"");
        serde_json::from_str(&normalized)
            .with_context(|| format!("invalid JSON from {route}: {text}"))
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let console = cli.console.as_deref().context("--console is required")?;
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

    let tokens: Vec<String> = files.iter().map(|_| random_token()).collect();
    let files_by_key: SharedFiles = Arc::new(
        tokens
            .iter()
            .cloned()
            .zip(files.iter().cloned())
            .collect(),
    );
    let socket = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )?;
    socket.set_send_buffer_size(4 << 20)?;
    socket
        .bind(&std::net::SocketAddr::from(([0, 0, 0, 0], port)).into())
        .map_err(|error| anyhow!("could not bind local file server on port {port}: {error}"))?;
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
                .with_state(files_by_key);
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
        wait_for_task(api, task_id, poll_seconds.max(1), human)
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

// Returns "done/total" with units; falls back to the raw text if it is not numeric.
fn format_progress(done: &str, total: &str, human: bool) -> String {
    match (human, parse_number(done), parse_number(total)) {
        (true, Some(d), Some(t)) => format!("{}/{}", human_bytes(d), human_bytes(t)),
        _ => format!("{done}/{total} bytes"),
    }
}

fn wait_for_task(api: &ApiClient, task_id: u32, poll_seconds: u64, human: bool) -> Result<()> {
    loop {
        thread::sleep(Duration::from_secs(poll_seconds));
        let value = api.post("/get_task_progress", json!({ "task_id": task_id }))?;
        let response: ApiResponse = serde_json::from_value(value.clone())?;
        ensure_success(&response)?;
        let progress: TaskProgress = serde_json::from_value(value)?;
        let status = task_status(progress.bits.as_ref());
        print!("\rTask {task_id}: {status}");
        if let (Some(done), Some(total)) = (&progress.transferred, &progress.length) {
            print!(" ({})", format_progress(done, total, human));
        }
        use std::io::Write;
        std::io::stdout().flush().ok();
        if status == "Installed" || status == "Stopped" {
            println!();
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
    match numeric {
        Some(0x18a) => "Downloading",
        Some(0x18e) => "Installing",
        Some(0x193) => "Installed",
        Some(0x1a8) => "Paused",
        Some(0x188) => "Stopped",
        _ => "Unknown",
    }
}

fn show_tasks(api: &ApiClient, human: bool) -> Result<()> {
    let ids = load_task_ids()?;
    if ids.is_empty() {
        println!("No tracked tasks. Start an install to add one.");
        return Ok(());
    }
    for id in ids {
        let value = api.post("/get_task_progress", json!({ "task_id": id }))?;
        let response: ApiResponse = serde_json::from_value(value.clone())?;
        ensure_success(&response)?;
        let task: TaskProgress = serde_json::from_value(value)?;
        let transferred = task.transferred.unwrap_or_else(|| "?".into());
        let length = task.length.unwrap_or_else(|| "?".into());
        println!(
            "{id}\t{}\t{}",
            task_status(task.bits.as_ref()),
            format_progress(&transferred, &length, human)
        );
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
