//! 从 GitHub Release 检查、更新或回滚 mk2api。
//!
//! 管理台左上角版本号会调这里：黄色表示有新版本，绿色表示已是最新。

use serde_json::{json, Value};
use std::{
    cmp::Ordering,
    error::Error,
    path::PathBuf,
    process::Command,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

type BoxError = Box<dyn Error + Send + Sync>;

const REPO: &str = "co0ontty/mk-to-api";
const LABEL: &str = "com.monkeycode.mk2api";
const CACHE_TTL: Duration = Duration::from_secs(60);

static CACHE: Mutex<Option<(Instant, Vec<Release>)>> = Mutex::const_new(None);

#[derive(Clone, Debug)]
struct Release {
    tag: String,
    name: String,
    published_at: String,
    prerelease: bool,
}

pub fn current_version() -> String {
    for path in version_paths() {
        if let Ok(text) = std::fs::read_to_string(path) {
            let value = text.trim();
            if !value.is_empty() && value != "latest" {
                return normalize_tag(value);
            }
        }
    }
    normalize_tag(env!("CARGO_PKG_VERSION"))
}

pub fn normalize_tag(value: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        return String::new();
    }
    if value.starts_with('v') || value.starts_with('V') {
        format!("v{}", value[1..].trim())
    } else if value.chars().next().is_some_and(|ch| ch.is_ascii_digit()) {
        format!("v{value}")
    } else {
        value.to_string()
    }
}

fn version_tuple(tag: &str) -> Option<(u64, u64, u64)> {
    let raw = tag.trim().trim_start_matches(['v', 'V']);
    let mut parts = raw.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

pub fn cmp_version(left: &str, right: &str) -> Option<Ordering> {
    Some(version_tuple(left)?.cmp(&version_tuple(right)?))
}

fn arch() -> Result<&'static str, BoxError> {
    let output = Command::new("uname").arg("-m").output()?;
    match String::from_utf8_lossy(&output.stdout).trim() {
        "arm64" | "aarch64" => Ok("aarch64"),
        "x86_64" | "amd64" => Ok("x86_64"),
        other => Err(format!("unsupported architecture: {other}").into()),
    }
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}

fn install_root() -> PathBuf {
    std::env::var("MONKEYCODE_INSTALL_ROOT")
        .or_else(|_| std::env::var("MK2API_INSTALL_ROOT"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".local/lib/mk2api"))
}

fn bin_dir() -> PathBuf {
    std::env::var("MK2API_BIN_DIR").map(PathBuf::from).unwrap_or_else(|_| home().join(".local/bin"))
}

fn version_paths() -> Vec<PathBuf> {
    vec![install_root().join("VERSION"), bin_dir().join("VERSION")]
}

async fn github_releases(client: &reqwest::Client) -> Result<Vec<Release>, BoxError> {
    {
        let cache = CACHE.lock().await;
        if let Some((at, items)) = cache.as_ref() {
            if at.elapsed() < CACHE_TTL {
                return Ok(items.clone());
            }
        }
    }
    let response = client
        .get(format!("https://api.github.com/repos/{REPO}/releases?per_page=30"))
        .header("User-Agent", "mk2api")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(format!("GitHub releases HTTP {}", response.status()).into());
    }
    let value: Value = response.json().await?;
    let releases = value
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| !item.get("draft").and_then(Value::as_bool).unwrap_or(false))
        .filter_map(|item| {
            let tag = item.get("tag_name").and_then(Value::as_str)?;
            Some(Release {
                tag: normalize_tag(tag),
                name: item.get("name").and_then(Value::as_str).unwrap_or(tag).to_string(),
                published_at: item.get("published_at").and_then(Value::as_str).unwrap_or("").to_string(),
                prerelease: item.get("prerelease").and_then(Value::as_bool).unwrap_or(false),
            })
        })
        .collect::<Vec<_>>();
    *CACHE.lock().await = Some((Instant::now(), releases.clone()));
    Ok(releases)
}

pub async fn status(client: &reqwest::Client) -> Value {
    let current = current_version();
    match github_releases(client).await {
        Ok(releases) => {
            let latest = releases
                .iter()
                .find(|item| !item.prerelease)
                .map(|item| item.tag.clone())
                .unwrap_or_else(|| current.clone());
            let update_available = cmp_version(&latest, &current).map(|order| order == Ordering::Greater).unwrap_or(false);
            json!({
                "current": current,
                "latest": latest,
                "update_available": update_available,
                "releases": releases.iter().map(|item| json!({
                    "tag": item.tag,
                    "name": item.name,
                    "published_at": item.published_at,
                    "prerelease": item.prerelease,
                    "current": item.tag == current,
                })).collect::<Vec<_>>(),
            })
        }
        Err(error) => json!({
            "current": current,
            "latest": Value::Null,
            "update_available": false,
            "releases": [],
            "error": error.to_string(),
        }),
    }
}

pub async fn apply(client: &reqwest::Client, requested: &str) -> Result<Value, BoxError> {
    let releases = github_releases(client).await?;
    let current = current_version();
    let tag = if requested.is_empty() || requested == "latest" {
        releases
            .iter()
            .find(|item| !item.prerelease)
            .map(|item| item.tag.clone())
            .ok_or_else(|| boxed("no stable release"))?
    } else {
        normalize_tag(requested)
    };
    if tag == current {
        return Ok(json!({"ok": true, "version": tag, "unchanged": true, "restarting": false}));
    }
    if !releases.iter().any(|item| item.tag == tag) {
        return Err(boxed(format!("unknown version: {tag}")));
    }
    install_tag(client, &tag).await?;
    schedule_restart();
    Ok(json!({"ok": true, "version": tag, "unchanged": false, "restarting": true, "previous": current}))
}

fn boxed(message: impl Into<String>) -> BoxError {
    std::io::Error::new(std::io::ErrorKind::Other, message.into()).into()
}

async fn install_tag(client: &reqwest::Client, tag: &str) -> Result<(), BoxError> {
    let arch = arch()?;
    let asset = format!("mk2api-macos-{arch}.tar.gz");
    let url = format!("https://github.com/{REPO}/releases/download/{tag}/{asset}");
    let bytes = client.get(&url).header("User-Agent", "mk2api").send().await?.error_for_status()?.bytes().await?;
    let tmp = std::env::temp_dir().join(format!("mk2api-update-{}", tag.trim_start_matches('v')));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;
    let archive = tmp.join(&asset);
    std::fs::write(&archive, &bytes)?;
    let status = Command::new("tar")
        .args(["-xzf", &archive.to_string_lossy(), "-C", &tmp.to_string_lossy()])
        .status()?;
    if !status.success() {
        return Err(boxed("failed to extract release archive"));
    }
    let extracted = ["mk2api", "monkeycode-direct-gateway"]
        .iter()
        .map(|name| tmp.join(name))
        .find(|path| path.is_file())
        .ok_or_else(|| boxed("release archive does not contain mk2api"))?;
    let root = install_root();
    let bin = bin_dir();
    std::fs::create_dir_all(&root)?;
    std::fs::create_dir_all(&bin)?;
    let dest = root.join("mk2api");
    std::fs::copy(&extracted, &dest)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::write(root.join("VERSION"), format!("{tag}\n"))?;
    let link = bin.join("mk2api");
    let _ = std::fs::remove_file(&link);
    #[cfg(unix)]
    std::os::unix::fs::symlink(&dest, &link)?;
    #[cfg(not(unix))]
    {
        std::fs::copy(&dest, &link)?;
    }
    let _ = std::fs::remove_dir_all(&tmp);
    Ok(())
}

fn schedule_restart() {
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(800));
        let uid = Command::new("/usr/bin/id")
            .arg("-u")
            .output()
            .ok()
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .and_then(|text| text.trim().parse::<u32>().ok())
            .unwrap_or(501);
        let target = format!("gui/{uid}/{LABEL}");
        let _ = Command::new("/bin/launchctl").args(["kickstart", "-k", &target]).status();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_and_normalizes_tags() {
        assert_eq!(normalize_tag("0.1.21"), "v0.1.21");
        assert_eq!(normalize_tag("v0.1.21"), "v0.1.21");
        assert_eq!(cmp_version("v0.1.21", "0.1.20"), Some(Ordering::Greater));
        assert_eq!(cmp_version("v0.1.19", "v0.1.21"), Some(Ordering::Less));
        assert_eq!(cmp_version("v0.1.21", "v0.1.21"), Some(Ordering::Equal));
    }
}
