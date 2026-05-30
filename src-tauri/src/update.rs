use std::time::Duration;

use reqwest::header::{ACCEPT, USER_AGENT};
use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Manager};

const LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/machaniconico/fast-comment/releases/latest";
const UPDATE_CHECK_USER_AGENT: &str = "fast-comment-update-check";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStatus {
    pub update_available: bool,
    pub current_version: String,
    pub latest_version: String,
    pub release_url: String,
}

impl UpdateStatus {
    fn unavailable(current_version: String) -> Self {
        Self {
            update_available: false,
            current_version,
            latest_version: String::new(),
            release_url: String::new(),
        }
    }
}

#[tauri::command]
pub async fn check_for_update(app: AppHandle) -> Result<UpdateStatus, String> {
    let current_version = app.package_info().version.to_string();
    let mut status = UpdateStatus::unavailable(current_version.clone());

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            tracing::warn!("更新チェック用 HTTP client の作成に失敗: {e}");
            return Ok(status);
        }
    };

    let response = match client
        .get(LATEST_RELEASE_URL)
        .header(USER_AGENT, UPDATE_CHECK_USER_AGENT)
        .header(ACCEPT, "application/vnd.github+json")
        .send()
        .await
    {
        Ok(response) => response,
        Err(e) => {
            tracing::warn!("GitHub Releases 更新チェックに失敗: {e}");
            return Ok(status);
        }
    };

    let http_status = response.status();
    if !http_status.is_success() {
        tracing::warn!("GitHub Releases 更新チェックが HTTP {http_status} を返しました");
        return Ok(status);
    }

    let body = match response.json::<Value>().await {
        Ok(body) => body,
        Err(e) => {
            tracing::warn!("GitHub Releases 更新チェックの JSON パースに失敗: {e}");
            return Ok(status);
        }
    };

    let latest_tag = body
        .get("tag_name")
        .and_then(Value::as_str)
        .map(str::to_string);
    let release_url = body
        .get("html_url")
        .and_then(Value::as_str)
        .map(str::to_string);

    let latest_tag = match latest_tag {
        Some(tag) if !tag.trim().is_empty() => tag,
        _ => {
            tracing::warn!("GitHub Releases 更新チェック結果に tag_name がありません");
            return Ok(status);
        }
    };

    let latest_version = latest_tag
        .strip_prefix('v')
        .unwrap_or(&latest_tag)
        .to_string();
    status.latest_version = latest_version.clone();
    status.release_url = release_url.unwrap_or_default();

    let current = match parse_version(&current_version) {
        Some(version) => version,
        None => {
            tracing::warn!("現在バージョンを X.Y.Z として解釈できません: {current_version}");
            return Ok(status);
        }
    };
    let latest = match parse_version(&latest_tag) {
        Some(version) => version,
        None => {
            tracing::warn!("最新リリースタグを X.Y.Z として解釈できません: {latest_tag}");
            return Ok(status);
        }
    };

    status.update_available = latest > current;
    Ok(status)
}

#[tauri::command]
pub fn open_url(url: String) -> Result<(), String> {
    let url = url.trim().to_string();
    if !is_safe_external_url(&url) {
        return Err("http/https URL のみ開けます".to_string());
    }

    open_url_impl(&url).map_err(|e| {
        tracing::warn!("URL を既定ブラウザで開けませんでした: {e}");
        e
    })
}

fn parse_version(input: &str) -> Option<[u64; 3]> {
    let s = input.trim().strip_prefix('v').unwrap_or(input.trim());
    let (major, rest) = parse_u64_prefix(s)?;
    let rest = rest.strip_prefix('.')?;
    let (minor, rest) = parse_u64_prefix(rest)?;
    let rest = rest.strip_prefix('.')?;
    let (patch, suffix) = parse_u64_prefix(rest)?;

    if suffix.is_empty() || suffix.starts_with('-') || suffix.starts_with('+') {
        Some([major, minor, patch])
    } else {
        None
    }
}

fn parse_u64_prefix(input: &str) -> Option<(u64, &str)> {
    let digit_end = input
        .char_indices()
        .take_while(|(_, ch)| ch.is_ascii_digit())
        .map(|(idx, ch)| idx + ch.len_utf8())
        .last()
        .unwrap_or(0);

    if digit_end == 0 {
        return None;
    }

    let (digits, rest) = input.split_at(digit_end);
    digits.parse::<u64>().ok().map(|value| (value, rest))
}

fn is_safe_external_url(url: &str) -> bool {
    let trimmed = url.trim();
    trimmed.starts_with("https://") || trimmed.starts_with("http://")
}

#[cfg(target_os = "windows")]
fn open_url_impl(url: &str) -> Result<(), String> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::{null, null_mut};

    #[link(name = "shell32")]
    extern "system" {
        fn ShellExecuteW(
            hwnd: *mut std::ffi::c_void,
            lp_operation: *const u16,
            lp_file: *const u16,
            lp_parameters: *const u16,
            lp_directory: *const u16,
            n_show_cmd: i32,
        ) -> isize;
    }

    fn wide(value: &str) -> Vec<u16> {
        OsStr::new(value).encode_wide().chain(Some(0)).collect()
    }

    let operation = wide("open");
    let file = wide(url);
    let result = unsafe {
        ShellExecuteW(
            null_mut(),
            operation.as_ptr(),
            file.as_ptr(),
            null(),
            null(),
            1,
        )
    };

    if result <= 32 {
        Err(format!("ShellExecuteW failed with code {result}"))
    } else {
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn open_url_impl(url: &str) -> Result<(), String> {
    std::process::Command::new("open")
        .arg(url)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn open_url_impl(url: &str) -> Result<(), String> {
    std::process::Command::new("xdg-open")
        .arg(url)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(not(any(target_os = "windows", target_os = "macos", unix)))]
fn open_url_impl(_url: &str) -> Result<(), String> {
    Err("この OS では URL オープンに対応していません".to_string())
}
