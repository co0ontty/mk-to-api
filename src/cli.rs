use serde_json::{json, Value};
use std::{
    error::Error,
    fs,
    io::{self, IsTerminal, Read, Write},
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

type BoxError = Box<dyn Error + Send + Sync>;

const DEFAULT_HOST: &str = "0.0.0.0";
const DEFAULT_PORT: u16 = 8123;
const LABEL: &str = "com.monkeycode.mk2api";

#[derive(Clone, Debug)]
struct MkConfig {
    host: String,
    port: u16,
    auth_required: bool,
}

pub async fn run(args: &[String]) -> Result<(), BoxError> {
    let command = args.first().map(String::as_str).unwrap_or("");
    match command {
        "start" => start().await,
        "stop" => stop(),
        "restart" => {
            let _ = stop();
            start().await
        }
        "status" => status().await,
        "tui" => tui().await,
        "install" => {
            let path = install_command()?;
            println!("registered mk2api at {}", path.display());
            Ok(())
        }
        "setup" => {
            let config = setup(true)?;
            println!(
                "wrote {} (http://{}:{})",
                config_path()?.display(),
                display_host(&config.host),
                config.port
            );
            Ok(())
        }
        "clients" => clients_command(&args.get(1..).unwrap_or(&[])).await,
        "help" | "-h" | "--help" => {
            print_help();
            Ok(())
        }
        "" => {
            install_command()?;
            if !health(&load_or_default()?).await {
                start().await?;
            }
            tui().await
        }
        other => {
            print_help();
            Err(boxed(format!("unknown command: {other}")))
        }
    }
}

fn boxed(message: impl Into<String>) -> BoxError {
    io::Error::new(io::ErrorKind::InvalidInput, message.into()).into()
}

fn home_dir() -> PathBuf {
    std::env::var("MK2API_HOME")
        .map(PathBuf::from)
        .ok()
        .or_else(|| std::env::var("HOME").ok().map(|home| PathBuf::from(home).join(".mk2api")))
        .unwrap_or_else(|| PathBuf::from(".mk2api"))
}

fn config_path() -> Result<PathBuf, BoxError> {
    Ok(std::env::var("MK2API_CONFIG")
        .or_else(|_| std::env::var("MONKEYCODE_GATEWAY_CONFIG"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| home_dir().join("config.json")))
}

fn port_path() -> PathBuf {
    home_dir().join("port")
}

fn admin_key_path() -> PathBuf {
    home_dir().join("admin.key")
}

fn plist_path() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

fn log_path() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
        .join("Library/Logs/mk2api.log")
}

fn bin_dir() -> PathBuf {
    std::env::var("MK2API_BIN_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".local/bin"))
}

fn display_host(host: &str) -> &str {
    match host {
        "0.0.0.0" | "::" | "[::]" => "127.0.0.1",
        other => other,
    }
}

fn print_help() {
    print!(
        "\
Usage:
  mk2api              Start the service if needed and open the usage dashboard
  mk2api start        Start the background launchd service
  mk2api stop         Stop the background service
  mk2api restart      Restart the background service
  mk2api status       Show service status
  mk2api tui          Open the usage dashboard
  mk2api setup        Create ~/.mk2api/config.json
  mk2api install      Install this binary as mk2api
  mk2api clients      Detect Pi/Codex configs and keep them pointed at mk2api
  mk2api clients sync Force-refresh managed Pi/Codex model catalogs
"
    );
}

fn prompt(label: &str, default: &str) -> Result<String, BoxError> {
    if !io::stdin().is_terminal() || std::env::var("MK2API_NONINTERACTIVE").ok().as_deref() == Some("1") {
        return Ok(default.to_string());
    }
    print!("{label} [{default}]: ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let value = line.trim();
    Ok(if value.is_empty() {
        default.to_string()
    } else {
        value.to_string()
    })
}

fn write_private(path: &Path, contents: &str) -> Result<(), BoxError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, contents)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

fn load_or_default() -> Result<MkConfig, BoxError> {
    let path = config_path()?;
    if path.exists() {
        let value: Value = serde_json::from_str(&fs::read_to_string(&path)?)?;
        return Ok(MkConfig {
            host: value
                .get("host")
                .and_then(Value::as_str)
                .unwrap_or(DEFAULT_HOST)
                .to_string(),
            port: value
                .get("port")
                .and_then(Value::as_u64)
                .or_else(|| fs::read_to_string(port_path()).ok().and_then(|v| v.trim().parse::<u64>().ok()))
                .unwrap_or(DEFAULT_PORT as u64) as u16,
            auth_required: value.get("auth_required").and_then(Value::as_bool).unwrap_or(true),
        });
    }
    Ok(MkConfig {
        host: DEFAULT_HOST.into(),
        port: fs::read_to_string(port_path())
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(DEFAULT_PORT),
        auth_required: true,
    })
}

fn setup(force_prompt: bool) -> Result<MkConfig, BoxError> {
    let path = config_path()?;
    if path.exists() && !force_prompt {
        return load_or_default();
    }
    println!("mk2api first-time setup  •  config: {}", path.display());
    println!("Press Enter to keep the default for each option.");
    let host = prompt("Listen host", DEFAULT_HOST)?;
    let port = prompt("Listen port", &DEFAULT_PORT.to_string())?
        .parse::<u16>()
        .map_err(|_| boxed("port must be a number"))?;
    let auth = prompt("Require API keys", "true")?;
    let auth_required = !matches!(auth.to_ascii_lowercase().as_str(), "false" | "0" | "no");
    let config = MkConfig {
        host,
        port,
        auth_required,
    };
    save_config(&config)?;
    Ok(config)
}

fn save_config(config: &MkConfig) -> Result<(), BoxError> {
    let path = config_path()?;
    let value = json!({
        "host": config.host,
        "port": config.port,
        "auth_required": config.auth_required,
        "api_keys_file": home_dir().join("api-keys.json").to_string_lossy(),
        "usage_file": home_dir().join("usage.json").to_string_lossy(),
        "admin_key_file": admin_key_path().to_string_lossy(),
    });
    write_private(&path, &serde_json::to_string_pretty(&value)?)?;
    write_private(&port_path(), &format!("{}\n", config.port))?;
    Ok(())
}

fn port_open(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok() && TcpListener::bind(("0.0.0.0", port)).is_ok()
}

fn select_free_port(preferred: u16) -> Result<u16, BoxError> {
    let end = preferred.saturating_add(100).min(65535);
    for port in preferred..=end {
        if port_open(port) {
            return Ok(port);
        }
    }
    Err(boxed(format!("no free port found from {preferred}")))
}

fn current_binary() -> Result<PathBuf, BoxError> {
    Ok(std::env::current_exe()?)
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn launchctl() -> Command {
    Command::new("/bin/launchctl")
}

fn service_target() -> String {
    format!("gui/{}/{}", users_uid(), LABEL)
}

fn users_uid() -> u32 {
    Command::new("/usr/bin/id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(501)
}

fn service_loaded() -> bool {
    launchctl().args(["print", &service_target()]).output().map(|o| o.status.success()).unwrap_or(false)
}

fn service_is_ours() -> bool {
    if !service_loaded() {
        return false;
    }
    let Ok(binary) = current_binary() else {
        return false;
    };
    let output = launchctl().args(["print", &service_target()]).output().ok();
    output
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(&binary.to_string_lossy().to_string())
            || String::from_utf8_lossy(&o.stdout).contains("mk2api"))
        .unwrap_or(false)
}

fn write_plist(config: &MkConfig, binary: &Path) -> Result<PathBuf, BoxError> {
    let plist = plist_path();
    if let Some(parent) = plist.parent() {
        fs::create_dir_all(parent)?;
    }
    let log = log_path();
    if let Some(parent) = log.parent() {
        fs::create_dir_all(parent)?;
    }
    let home = home_dir();
    let body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{binary}</string>
    <string>serve</string>
  </array>
  <key>WorkingDirectory</key>
  <string>{workdir}</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>MK2API_HOME</key>
    <string>{home}</string>
    <key>MK2API_CONFIG</key>
    <string>{config}</string>
    <key>DIRECT_GATEWAY_HOST</key>
    <string>{host}</string>
    <key>DIRECT_GATEWAY_PORT</key>
    <string>{port}</string>
  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>ThrottleInterval</key>
  <integer>5</integer>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#,
        binary = xml_escape(&binary.to_string_lossy()),
        workdir = xml_escape(&binary.parent().unwrap_or(Path::new("/")).to_string_lossy()),
        home = xml_escape(&home.to_string_lossy()),
        config = xml_escape(&config_path()?.to_string_lossy()),
        host = xml_escape(&config.host),
        port = config.port,
        log = xml_escape(&log.to_string_lossy()),
    );
    write_private(&plist, &body)?;
    Ok(plist)
}

fn install_command() -> Result<PathBuf, BoxError> {
    let dest = bin_dir().join("mk2api");
    fs::create_dir_all(bin_dir())?;
    let src = current_binary()?;
    if src != dest {
        fs::copy(&src, &dest)?;
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o755))?;
    }
    Ok(dest)
}

async fn health(config: &MkConfig) -> bool {
    let url = format!("http://{}:{}/health", display_host(&config.host), config.port);
    let Ok(client) = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
    else {
        return false;
    };
    let Ok(response) = client.get(url).send().await else {
        return false;
    };
    let Ok(text) = response.text().await else {
        return false;
    };
    text.contains("\"ok\":true") && text.contains("direct-signed-gateway")
}

async fn start() -> Result<(), BoxError> {
    let mut config = setup(false)?;
    if service_is_ours() && health(&config).await {
        println!(
            "mk2api is already running on http://{}:{}",
            display_host(&config.host),
            config.port
        );
        return Ok(());
    }
    if service_is_ours() {
        let _ = launchctl().args(["bootout", &service_target()]).status();
        thread::sleep(Duration::from_millis(300));
    } else if service_loaded() {
        return Err(boxed(format!(
            "launchd label {LABEL} is already used by another program"
        )));
    }
    let selected = select_free_port(config.port)?;
    if selected != config.port {
        println!("port {} is occupied; using available port {selected}", config.port);
        config.port = selected;
        save_config(&config)?;
    }
    let binary = install_command()?;
    let plist = write_plist(&config, &binary)?;
    let domain = format!("gui/{}", users_uid());
    let status = launchctl()
        .args(["bootstrap", &domain, &plist.to_string_lossy()])
        .status()?;
    if !status.success() {
        return Err(boxed("launchd bootstrap failed"));
    }
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(8) {
        if health(&config).await {
            println!(
                "mk2api is running on http://{}:{}",
                display_host(&config.host),
                config.port
            );
            println!("admin: http://{}:{}/admin", display_host(&config.host), config.port);
            println!("config: {}", config_path()?.display());
            println!("log: {}", log_path().display());
            return Ok(());
        }
        thread::sleep(Duration::from_millis(200));
    }
    Err(boxed(format!(
        "failed to start mk2api; see {}",
        log_path().display()
    )))
}

fn stop() -> Result<(), BoxError> {
    if service_is_ours() {
        let _ = launchctl().args(["bootout", &service_target()]).status();
        let _ = fs::remove_file(plist_path());
        println!("mk2api stopped");
        Ok(())
    } else if service_loaded() {
        Err(boxed(format!(
            "launchd label {LABEL} is used by another program; not stopping it"
        )))
    } else {
        println!("mk2api is not running");
        Ok(())
    }
}

async fn status() -> Result<(), BoxError> {
    let config = load_or_default()?;
    if service_is_ours() && health(&config).await {
        println!(
            "mk2api is running on http://{}:{}",
            display_host(&config.host),
            config.port
        );
        return Ok(());
    }
    if service_loaded() {
        println!("mk2api service is loaded but unhealthy");
        let _ = launchctl().args(["print", &service_target()]).status();
        return Err(boxed("unhealthy"));
    }
    println!("mk2api is not running");
    Err(boxed("not running"))
}

async fn clients_command(args: &[String]) -> Result<(), BoxError> {
    let config = load_or_default()?;
    if !health(&config).await {
        return Err(boxed("mk2api is not running. Try: mk2api start"));
    }
    let admin = admin_key()?;
    let sync = args.first().map(String::as_str) == Some("sync");
    let url = format!(
        "http://{}:{}/v1/admin/clients{}",
        display_host(&config.host),
        config.port,
        if sync { "/sync" } else { "" }
    );
    let client = reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(10)).build()?;
    let request = if sync { client.post(&url) } else { client.get(&url) };
    let response = request.header("Authorization", format!("Bearer {admin}")).send().await?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(boxed(format!("client sync failed ({status}): {body}")));
    }
    let data: Value = response.json().await?;
    println!(
        "mk2api client hosting  •  {}",
        data.get("base_url").and_then(Value::as_str).unwrap_or("-")
    );
    println!(
        "catalog {:>5} models   manage_clients={}",
        data.get("catalog").and_then(Value::as_u64).unwrap_or(0),
        data.get("enabled").and_then(Value::as_bool).unwrap_or(false)
    );
    if let Some(items) = data.get("clients").and_then(Value::as_array) {
        for item in items {
            let name = item.get("name").and_then(Value::as_str).unwrap_or("unknown");
            let detected = item.get("detected").and_then(Value::as_bool).unwrap_or(false);
            let managed = item.get("managed").and_then(Value::as_bool).unwrap_or(false);
            let models = item.get("models").and_then(Value::as_u64).unwrap_or(0);
            let path = item.get("path").and_then(Value::as_str).unwrap_or("-");
            let message = item.get("message").and_then(Value::as_str).unwrap_or("");
            let state = if managed {
                "managed"
            } else if detected {
                "detected"
            } else {
                "skipped"
            };
            println!("  {name:<6} {state:<9} {models:>3} models  {path}  {message}");
        }
    }
    Ok(())
}

fn admin_key() -> Result<String, BoxError> {
    if let Ok(value) = std::env::var("DIRECT_GATEWAY_ADMIN_KEY") {
        if !value.trim().is_empty() {
            return Ok(value);
        }
    }
    let path = admin_key_path();
    if path.exists() {
        return Ok(fs::read_to_string(path)?.trim().to_string());
    }
    Err(boxed("Admin Key is unavailable; start mk2api first"))
}

async fn tui() -> Result<(), BoxError> {
    let config = load_or_default()?;
    if !health(&config).await {
        return Err(boxed("mk2api is not running. Try: mk2api start"));
    }
    let admin = admin_key()?;
    let url = format!(
        "http://{}:{}/v1/admin/usage",
        display_host(&config.host),
        config.port
    );
    let client = reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(5)).build()?;
    let interactive = io::stdout().is_terminal() && io::stdin().is_terminal();
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    if interactive {
        thread::spawn(move || {
            let mut stdin = io::stdin();
            let mut buf = [0u8; 1];
            while stdin.read(&mut buf).is_ok() {
                if buf[0] == b'q' || buf[0] == b'Q' {
                    let _ = tx.send(());
                    break;
                }
            }
        });
    }
    loop {
        let response = client
            .get(&url)
            .header("Authorization", format!("Bearer {admin}"))
            .send()
            .await?;
        let data: Value = response.json().await?;
        if interactive {
            print!("\x1b[2J\x1b[H");
        }
        println!(
            "mk2api usage dashboard  •  http://{}:{}",
            display_host(&config.host),
            config.port
        );
        if interactive {
            println!("Press q to quit; refreshes every 5 seconds.\n");
        }
        println!("Requests      {:>10}", data.get("requests").and_then(Value::as_u64).unwrap_or(0));
        println!("Input tokens  {:>10}", data.get("input_tokens").and_then(Value::as_u64).unwrap_or(0));
        println!("Output tokens {:>10}", data.get("output_tokens").and_then(Value::as_u64).unwrap_or(0));
        println!("Total tokens  {:>10}", data.get("total_tokens").and_then(Value::as_u64).unwrap_or(0));
        for (title, key) in [("By model", "by_model"), ("By API key", "by_key")] {
            println!("\n{title}");
            if let Some(items) = data.get(key).and_then(Value::as_array) {
                if items.is_empty() {
                    println!("  (none)");
                }
                for item in items {
                    let name = item.get("name").and_then(Value::as_str).unwrap_or("unknown");
                    let requests = item.get("requests").and_then(Value::as_u64).unwrap_or(0);
                    let tokens = item.get("input_tokens").and_then(Value::as_u64).unwrap_or(0)
                        + item.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
                    let errors = item.get("errors").and_then(Value::as_u64).unwrap_or(0);
                    println!("  {name}: {requests} requests, {tokens} tokens, {errors} errors");
                }
            }
        }
        if !interactive {
            break;
        }
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
    Ok(())
}
