//! Namespaced template storage. Builtins are never mutation targets.
use serde::Serialize;
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

pub const MAX_EDITABLE_BYTES: u64 = 1024 * 1024;
// Same as the zip import limits so an imported template (e.g. with video) can be duplicated.
pub const MAX_FILE_BYTES: u64 = 100 * 1024 * 1024;
pub const MAX_TOTAL_BYTES: u64 = 100 * 1024 * 1024;
pub const MAX_ENTRIES: usize = 500;
pub const MAX_DEPTH: usize = 4;
pub const EDITABLE_FILES: &[&str] = &["index.html", "style.css", "app.js"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateRef {
    Builtin(String),
    Custom(String),
}

impl TemplateRef {
    pub fn parse(id: &str) -> Result<Self, String> {
        let (custom, name) = match id.strip_prefix("custom:") {
            Some(name) => (true, name),
            None => (false, id),
        };
        if !crate::bus::is_valid_template_name(name) || (!custom && name == "custom") {
            return Err("テンプレート ID が不正です".into());
        }
        Ok(if custom {
            Self::Custom(name.into())
        } else {
            Self::Builtin(name.into())
        })
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Builtin(name) | Self::Custom(name) => name,
        }
    }

    pub fn id(&self) -> String {
        match self {
            Self::Builtin(name) => name.clone(),
            Self::Custom(name) => format!("custom:{name}"),
        }
    }

    pub fn url_base(&self) -> String {
        match self {
            Self::Builtin(name) => format!("/{name}/"),
            Self::Custom(name) => format!("/custom/{name}/"),
        }
    }

    fn info(&self) -> TemplateInfo {
        TemplateInfo {
            id: self.id(),
            name: self.name().into(),
            kind: if matches!(self, Self::Builtin(_)) {
                "builtin"
            } else {
                "custom"
            }
            .into(),
            editable: matches!(self, Self::Custom(_)),
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TemplateInfo {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub editable: bool,
}

/// A private, unpublished directory; any unsuccessful operation removes it.
#[derive(Debug)]
pub struct StagedTemplate {
    path: PathBuf,
}

impl StagedTemplate {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for StagedTemplate {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[derive(Debug, Clone)]
pub struct TemplateStore {
    builtin_root: PathBuf,
    custom_root: PathBuf,
    staging_root: PathBuf,
    mutation: Arc<Mutex<()>>,
}

fn io_error(error: impl std::fmt::Display) -> String {
    error.to_string()
}

// Canonicalize the existing ancestor too, so aliases of a not-yet-created root
// cannot bypass the identical-roots guard.
fn real_path(path: &Path) -> Result<PathBuf, String> {
    if path.exists() {
        return fs::canonicalize(path).map_err(io_error);
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path.file_name().ok_or("ルートパスが不正です")?;
    Ok(real_path(parent)?.join(name))
}

impl TemplateStore {
    /// custom_root is app_config_dir/templates; staging is its sibling.
    pub fn new(builtin_root: PathBuf, custom_root: PathBuf) -> Self {
        let staging_root = custom_root
            .parent()
            .unwrap_or(Path::new("."))
            .join("templates-staging");
        Self {
            builtin_root,
            custom_root,
            staging_root,
            mutation: Arc::new(Mutex::new(())),
        }
    }

    pub fn builtin_root(&self) -> &Path {
        &self.builtin_root
    }
    pub fn custom_root(&self) -> &Path {
        &self.custom_root
    }
    pub fn staging_root(&self) -> &Path {
        &self.staging_root
    }

    pub fn custom_enabled(&self) -> bool {
        match (real_path(&self.builtin_root), real_path(&self.custom_root)) {
            (Ok(builtin), Ok(custom)) => {
                !builtin.starts_with(&custom) && !custom.starts_with(&builtin)
            }
            _ => false,
        }
    }

    fn require_custom(&self) -> Result<(), String> {
        if !self.custom_enabled() {
            return Err("内蔵とカスタムの保存先が重複しているか利用できません".into());
        }
        Ok(())
    }

    pub fn list(&self) -> Result<Vec<TemplateInfo>, String> {
        let mut result = Vec::new();
        for (root, custom) in [(&self.builtin_root, false), (&self.custom_root, true)] {
            if custom && !self.custom_enabled() {
                continue;
            }
            let entries = match fs::read_dir(root) {
                Ok(entries) => entries,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) if custom => {
                    tracing::warn!("カスタムテンプレート一覧を読み込めません: {e}");
                    continue;
                }
                Err(e) => return Err(io_error(e)),
            };
            let mut group = Vec::new();
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(e) if custom => {
                        tracing::warn!("カスタムテンプレートのエントリを読み込めません: {e}");
                        continue;
                    }
                    Err(e) => return Err(io_error(e)),
                };
                let name = entry.file_name().to_string_lossy().into_owned();
                if TemplateRef::parse(&if custom {
                    format!("custom:{name}")
                } else {
                    name.clone()
                })
                .is_err()
                {
                    continue;
                }
                let kind = match entry.file_type() {
                    Ok(kind) => kind,
                    Err(e) if custom => {
                        tracing::warn!(path = %entry.path().display(), "カスタムテンプレートの種類を取得できません: {e}");
                        continue;
                    }
                    Err(e) => return Err(io_error(e)),
                };
                if !kind.is_dir() {
                    continue;
                }
                if checked_file(&entry.path(), "index.html").is_err() {
                    continue;
                }
                group.push(
                    if custom {
                        TemplateRef::Custom(name)
                    } else {
                        TemplateRef::Builtin(name)
                    }
                    .info(),
                );
            }
            group.sort_by(|a, b| a.name.cmp(&b.name));
            result.extend(group);
        }
        Ok(result)
    }

    pub fn resolve(&self, template: &TemplateRef) -> Result<PathBuf, String> {
        // Validate even directly constructed enum values.
        if TemplateRef::parse(&template.id())? != *template {
            return Err("テンプレート ID の名前空間が不正です".into());
        }
        let root = match template {
            TemplateRef::Builtin(_) => &self.builtin_root,
            TemplateRef::Custom(_) => {
                self.require_custom()?;
                &self.custom_root
            }
        };
        let path = root.join(template.name());
        let meta = fs::symlink_metadata(&path).map_err(io_error)?;
        if !meta.is_dir() || meta.file_type().is_symlink() {
            return Err("テンプレートは通常のディレクトリである必要があります".into());
        }
        Ok(path)
    }

    pub fn read(&self, template: &TemplateRef, file: &str) -> Result<String, String> {
        editable(file)?;
        let path = checked_file(&self.resolve(template)?, file)?;
        let mut bytes = Vec::new();
        File::open(path)
            .map_err(io_error)?
            .take(MAX_EDITABLE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(io_error)?;
        if bytes.len() as u64 > MAX_EDITABLE_BYTES {
            return Err("編集ファイルは 1MiB 以下にしてください".into());
        }
        String::from_utf8(bytes).map_err(io_error)
    }

    pub fn write(&self, template: &TemplateRef, file: &str, content: &str) -> Result<(), String> {
        if matches!(template, TemplateRef::Builtin(_)) {
            return Err("内蔵テンプレートは読み取り専用です。複製して編集してください".into());
        }
        self.require_custom()?;
        editable(file)?;
        if content.len() as u64 > MAX_EDITABLE_BYTES {
            return Err("編集ファイルは 1MiB 以下にしてください".into());
        }
        let _lock = self.mutation.lock().map_err(io_error)?;
        let path = checked_file(&self.resolve(template)?, file)?;
        let temporary = path.with_file_name(format!(".edit-{}.tmp", uuid::Uuid::new_v4()));
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(io_error)?;
        let result = (|| {
            output.write_all(content.as_bytes()).map_err(io_error)?;
            output.sync_all().map_err(io_error)?;
            // Close before rename so Windows can atomically replace the destination too.
            drop(output);
            fs::rename(&temporary, &path).map_err(io_error)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    pub fn validate_new_name(&self, name: &str) -> Result<(), String> {
        self.require_custom()?;
        if name.is_empty()
            || name.len() > 64
            || !name.as_bytes()[0].is_ascii_lowercase() && !name.as_bytes()[0].is_ascii_digit()
            || !name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
            || is_windows_device_name(name)
        {
            return Err("カスタムテンプレート名が不正です".into());
        }
        match fs::read_dir(&self.custom_root) {
            Ok(entries) => {
                for entry in entries {
                    if entry
                        .map_err(io_error)?
                        .file_name()
                        .to_string_lossy()
                        .eq_ignore_ascii_case(name)
                    {
                        return Err("同名のカスタムテンプレートが既にあります".into());
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(io_error(e)),
        }
        Ok(())
    }

    pub fn create_staging(&self) -> Result<StagedTemplate, String> {
        self.require_custom()?;
        self.check_staging_root()?;
        fs::create_dir_all(&self.staging_root).map_err(io_error)?;
        let path = self.staging_root.join(uuid::Uuid::new_v4().to_string());
        fs::create_dir(&path).map_err(io_error)?;
        Ok(StagedTemplate { path })
    }

    fn check_staging_root(&self) -> Result<(), String> {
        let staging = real_path(&self.staging_root)?;
        for root in [&self.builtin_root, &self.custom_root] {
            let root = real_path(root)?;
            if staging.starts_with(&root) || root.starts_with(&staging) {
                return Err("staging とテンプレートの保存先が重複しています".into());
            }
        }
        if let Ok(meta) = fs::symlink_metadata(&self.staging_root) {
            if meta.file_type().is_symlink() || !meta.is_dir() {
                return Err("staging は通常のディレクトリである必要があります".into());
            }
        }
        Ok(())
    }

    pub fn write_provenance(
        &self,
        staged: &StagedTemplate,
        origin: &str,
        source: &str,
    ) -> Result<(), String> {
        self.check_stage(staged)?;
        let metadata = serde_json::json!({"schema": 1, "origin": origin, "source": source,
            "appVersion": env!("CARGO_PKG_VERSION"),
            "createdAtMs": SystemTime::now().duration_since(UNIX_EPOCH).map_err(io_error)?.as_millis() as u64});
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(staged.path.join("fc-template.json"))
            .map_err(io_error)?;
        file.write_all(
            serde_json::to_string_pretty(&metadata)
                .map_err(io_error)?
                .as_bytes(),
        )
        .map_err(io_error)
    }

    fn check_stage(&self, staged: &StagedTemplate) -> Result<(), String> {
        self.require_custom()?;
        self.check_staging_root()?;
        if staged.path.parent() != Some(self.staging_root.as_path()) {
            return Err("別の保存先の staging です".into());
        }
        let meta = fs::symlink_metadata(&staged.path).map_err(io_error)?;
        if !meta.is_dir() || meta.file_type().is_symlink() {
            return Err("staging が不正です".into());
        }
        Ok(())
    }

    /// Consumes the guard: failure also removes the unpublished directory.
    pub fn commit_staged(
        &self,
        staged: StagedTemplate,
        name: &str,
    ) -> Result<TemplateInfo, String> {
        let _lock = self.mutation.lock().map_err(io_error)?;
        self.validate_new_name(name)?;
        self.check_stage(&staged)?;
        checked_file(&staged.path, "index.html")?;
        fs::create_dir_all(&self.custom_root).map_err(io_error)?;
        fs::rename(&staged.path, self.custom_root.join(name)).map_err(io_error)?;
        Ok(TemplateRef::Custom(name.into()).info())
    }

    pub fn duplicate(&self, source: &TemplateRef, name: &str) -> Result<TemplateInfo, String> {
        self.validate_new_name(name)?;
        let source_dir = self.resolve(source)?;
        let staged = self.create_staging()?;
        let mut budget = CopyBudget::default();
        copy_assets(&source_dir, staged.path(), Path::new(""), &mut budget)?;
        checked_file(staged.path(), "index.html")?;
        self.write_provenance(&staged, "duplicate", &source.id())?;
        self.commit_staged(staged, name)
    }

    pub fn delete(&self, template: &TemplateRef) -> Result<(), String> {
        if matches!(template, TemplateRef::Builtin(_)) {
            return Err("内蔵テンプレートは読み取り専用です".into());
        }
        self.require_custom()?;
        let _lock = self.mutation.lock().map_err(io_error)?;
        fs::remove_dir_all(self.resolve(template)?).map_err(io_error)
    }

    /// Call at startup, before accepting imports/duplicates.
    pub fn cleanup_stale_staging(&self) -> Result<(), String> {
        self.check_staging_root()?;
        let _lock = self.mutation.lock().map_err(io_error)?;
        let entries = match fs::read_dir(&self.staging_root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(io_error(e)),
        };
        for entry in entries {
            let entry = entry.map_err(io_error)?;
            let metadata = fs::symlink_metadata(entry.path()).map_err(io_error)?;
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                // Directory symlinks and junctions must be unlinked with remove_dir;
                // never recurse into their targets.
                if metadata.file_attributes() & 0x410 == 0x410 {
                    fs::remove_dir(entry.path()).map_err(io_error)?;
                    continue;
                }
            }
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                fs::remove_dir_all(entry.path()).map_err(io_error)?;
            } else {
                fs::remove_file(entry.path()).map_err(io_error)?;
            }
        }
        Ok(())
    }
}

fn editable(file: &str) -> Result<(), String> {
    if EDITABLE_FILES.contains(&file) {
        Ok(())
    } else {
        Err("編集できるファイルではありません".into())
    }
}

fn checked_file(directory: &Path, file: &str) -> Result<PathBuf, String> {
    let path = directory.join(file);
    let meta = fs::symlink_metadata(&path).map_err(io_error)?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        return Err("通常ファイルである必要があります".into());
    }
    Ok(path)
}

pub fn is_windows_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("").to_ascii_lowercase();
    matches!(stem.as_str(), "con" | "prn" | "aux" | "nul")
        || ((stem.starts_with("com") || stem.starts_with("lpt"))
            && stem.len() == 4
            && matches!(stem.as_bytes()[3], b'1'..=b'9'))
}

fn allowed_component(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && !name.ends_with('.')
        && !is_windows_device_name(name)
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Shared policy for slash-separated relative asset names. No filesystem access.
pub fn is_allowed_asset(name: &str) -> bool {
    if name.contains('\\') || name.split('/').any(|part| part == "." || part == "..") {
        return false;
    }
    let path = Path::new(name);
    let mut depth = 0;
    for component in path.components() {
        let Component::Normal(name) = component else {
            return false;
        };
        let Some(name) = name.to_str() else {
            return false;
        };
        if !allowed_component(name) {
            return false;
        }
        depth += 1;
    }
    if depth == 0 || depth > MAX_DEPTH {
        return false;
    }
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if ["fc-template.json", "thumbs.db", "desktop.ini"]
        .iter()
        .any(|n| name.eq_ignore_ascii_case(n))
    {
        return false;
    }
    matches!(
        path.extension()
            .and_then(|s| s.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some(
            "html"
                | "css"
                | "js"
                | "json"
                | "png"
                | "jpg"
                | "jpeg"
                | "gif"
                | "webp"
                | "svg"
                | "woff"
                | "woff2"
                | "ttf"
                | "otf"
                | "mp3"
                | "ogg"
                | "wav"
                | "mp4"
                | "webm"
                | "txt"
                | "md"
        )
    )
}

#[derive(Default)]
struct CopyBudget {
    entries: usize,
    bytes: u64,
    paths: HashSet<String>,
}

impl CopyBudget {
    fn register_path(&mut self, path: &Path) -> Result<(), String> {
        let normalized = path
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_ascii_lowercase())
            .collect::<Vec<_>>()
            .join("/");
        if !self.paths.insert(normalized) {
            return Err("大文字小文字を無視すると重複する資産パスがあります".into());
        }
        Ok(())
    }
}

fn copy_assets(
    source: &Path,
    destination: &Path,
    relative: &Path,
    budget: &mut CopyBudget,
) -> Result<(), String> {
    for entry in fs::read_dir(source.join(relative)).map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        budget.entries += 1;
        if budget.entries > MAX_ENTRIES {
            return Err("テンプレートのエントリ数が上限を超えています".into());
        }
        let kind = entry.file_type().map_err(io_error)?;
        if kind.is_symlink() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !allowed_component(name) || name.eq_ignore_ascii_case("__MACOSX") {
            continue;
        }
        let path = relative.join(name);
        if kind.is_dir() {
            if path.components().count() >= MAX_DEPTH {
                // Duplication skips and reports disallowed assets (including depth),
                // just like disallowed extensions; size/count/collisions remain fatal.
                tracing::warn!(path = %path.display(), "階層が深すぎる資産をスキップしました");
                continue;
            }
            budget.register_path(&path)?;
            fs::create_dir_all(destination.join(&path)).map_err(io_error)?;
            copy_assets(source, destination, &path, budget)?;
        } else if kind.is_file()
            && is_allowed_asset(
                &path
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/"),
            )
        {
            budget.register_path(&path)?;
            let limit = if relative.as_os_str().is_empty() && EDITABLE_FILES.contains(&name) {
                MAX_EDITABLE_BYTES
            } else {
                MAX_FILE_BYTES
            };
            let remaining = MAX_TOTAL_BYTES.saturating_sub(budget.bytes);
            let mut input = File::open(entry.path())
                .map_err(io_error)?
                .take(limit.min(remaining) + 1);
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(destination.join(&path))
                .map_err(io_error)?;
            let bytes = std::io::copy(&mut input, &mut output).map_err(io_error)?;
            if bytes > limit || bytes > remaining {
                return Err("テンプレートのサイズが上限を超えています".into());
            }
            budget.bytes += bytes;
        } else if kind.is_file() {
            tracing::warn!(path = %path.display(), "許可外の資産をスキップしました");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        root: PathBuf,
        store: TemplateStore,
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir()
                .join(format!("fast-comment-templates-{}", uuid::Uuid::new_v4()));
            let store = TemplateStore::new(root.join("builtin"), root.join("config/templates"));
            fs::create_dir_all(store.builtin_root()).unwrap();
            fs::create_dir_all(store.custom_root()).unwrap();
            Self { root, store }
        }
        fn template(&self, custom: bool, name: &str) -> TemplateRef {
            let root = if custom {
                self.store.custom_root()
            } else {
                self.store.builtin_root()
            };
            let dir = root.join(name);
            fs::create_dir_all(&dir).unwrap();
            for file in EDITABLE_FILES {
                fs::write(dir.join(file), format!("original {file}")).unwrap();
            }
            if custom {
                TemplateRef::Custom(name.into())
            } else {
                TemplateRef::Builtin(name.into())
            }
        }
        fn staging_empty(&self) {
            assert!(fs::read_dir(self.store.staging_root())
                .map(|mut e| e.next().is_none())
                .unwrap_or(true));
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    #[cfg(unix)]
    fn resolve_rejects_mismatched_namespace() {
        let f = Fixture::new();
        f.template(true, "foo");
        // Ensure filesystem lookup succeeds if the namespace roundtrip guard is removed.
        fs::create_dir_all(f.store.builtin_root().join("custom:foo")).unwrap();
        assert!(f
            .store
            .resolve(&TemplateRef::Builtin("custom:foo".into()))
            .is_err());
        assert!(f
            .store
            .resolve(&TemplateRef::Custom("../foo".into()))
            .is_err());
    }

    #[test]
    fn template_ref_reserves_custom_builtin() {
        assert!(TemplateRef::parse("custom").is_err());
        assert_eq!(
            TemplateRef::parse("custom:custom").unwrap().url_base(),
            "/custom/custom/"
        );
        let f = Fixture::new();
        let reserved = f.template(false, "custom");
        f.template(true, "custom");
        assert!(f.store.resolve(&reserved).is_err());
        let ids: Vec<_> = f.store.list().unwrap().into_iter().map(|t| t.id).collect();
        assert_eq!(ids, ["custom:custom"]);
    }

    #[test]
    fn nested_roots_disable_custom_in_both_directions() {
        let f = Fixture::new();
        let builtin = f.template(false, "default");
        for (builtin_root, custom_root) in [
            (
                f.store.builtin_root().to_path_buf(),
                f.store.builtin_root().join("nested"),
            ),
            (f.store.builtin_root().to_path_buf(), f.root.clone()),
        ] {
            let store = TemplateStore::new(builtin_root, custom_root);
            assert!(!store.custom_enabled());
            assert_eq!(store.list().unwrap().len(), 1);
            assert!(store.duplicate(&builtin, "copy").is_err());
            let custom = TemplateRef::Custom("default".into());
            assert!(store.write(&custom, "app.js", "changed").is_err());
            assert!(store.delete(&custom).is_err());
            assert!(store.create_staging().is_err());
        }
        assert_eq!(f.store.read(&builtin, "app.js").unwrap(), "original app.js");
    }

    #[test]
    fn list_preserves_builtins_when_custom_read_dir_fails() {
        let f = Fixture::new();
        f.template(false, "default");
        fs::remove_dir(f.store.custom_root()).unwrap();
        fs::write(f.store.custom_root(), "not a directory").unwrap();
        let list = f.store.list().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "default");
    }

    #[test]
    fn copy_budget_rejects_case_insensitive_relative_paths() {
        let mut budget = CopyBudget::default();
        budget.register_path(Path::new("index.html")).unwrap();
        assert!(budget.register_path(Path::new("Index.HTML")).is_err());
        budget.register_path(Path::new("Assets/image.png")).unwrap();
        assert!(budget.register_path(Path::new("assets/IMAGE.PNG")).is_err());
        budget.register_path(Path::new("other/image.png")).unwrap();
        budget.register_path(Path::new("Icons")).unwrap();
        assert!(budget.register_path(Path::new("icons")).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn duplicate_rejects_case_insensitive_files_and_directories() {
        let f = Fixture::new();
        let t = f.template(false, "default");
        let source = f.store.resolve(&t).unwrap();
        fs::write(source.join("Index.HTML"), "collision").unwrap();
        assert!(f.store.duplicate(&t, "copy").is_err());
        assert!(!f.store.custom_root().join("copy").exists());
        f.staging_empty();
        fs::remove_file(source.join("Index.HTML")).unwrap();
        fs::create_dir(source.join("Images")).unwrap();
        fs::create_dir(source.join("images")).unwrap();
        assert!(f.store.duplicate(&t, "copy").is_err());
        f.staging_empty();
    }

    #[test]
    fn duplicate_skips_too_deep_directories() {
        let f = Fixture::new();
        let t = f.template(false, "default");
        let source = f.store.resolve(&t).unwrap();
        fs::create_dir_all(source.join("a/b/c/d")).unwrap();
        fs::write(source.join("a/b/c/allowed.png"), "kept").unwrap();
        fs::write(source.join("a/b/c/d/skipped.png"), "skipped").unwrap();
        f.store.duplicate(&t, "copy").unwrap();
        let dest = f.store.custom_root().join("copy");
        assert_eq!(
            fs::read_to_string(dest.join("a/b/c/allowed.png")).unwrap(),
            "kept"
        );
        assert!(!dest.join("a/b/c/d").exists());
        f.staging_empty();
    }

    #[test]
    fn duplicate_accepts_64_character_name() {
        let f = Fixture::new();
        let t = f.template(false, "default");
        let name = "a".repeat(64);
        assert_eq!(
            f.store.duplicate(&t, &name).unwrap().id,
            format!("custom:{name}")
        );
    }

    #[test]
    fn duplicate_accepts_exact_max_file_and_total() {
        let f = Fixture::new();
        let t = f.template(false, "default");
        let source = f.store.resolve(&t).unwrap();
        // Empty editable files make the copied total exactly MAX_TOTAL_BYTES.
        for name in EDITABLE_FILES {
            fs::write(source.join(name), "").unwrap();
        }
        assert_eq!(MAX_FILE_BYTES, MAX_TOTAL_BYTES);
        File::create(source.join("asset.mp4"))
            .unwrap()
            .set_len(MAX_FILE_BYTES)
            .unwrap();
        f.store.duplicate(&t, "copy").unwrap();
        let dest = f.store.custom_root().join("copy");
        assert_eq!(
            fs::metadata(dest.join("asset.mp4")).unwrap().len(),
            MAX_TOTAL_BYTES
        );
        f.staging_empty();
    }

    #[test]
    fn duplicate_rejects_asset_over_max_file_by_one_byte() {
        let f = Fixture::new();
        let t = f.template(false, "default");
        let source = f.store.resolve(&t).unwrap();
        File::create(source.join("asset.png"))
            .unwrap()
            .set_len(MAX_FILE_BYTES + 1)
            .unwrap();
        assert!(f.store.duplicate(&t, "copy").is_err());
        assert!(!f.store.custom_root().join("copy").exists());
        f.staging_empty();
        assert_eq!(
            fs::metadata(source.join("asset.png")).unwrap().len(),
            MAX_FILE_BYTES + 1
        );
    }

    #[test]
    fn duplicate_rejects_total_over_max_by_one_byte() {
        let f = Fixture::new();
        let t = f.template(false, "default");
        let source = f.store.resolve(&t).unwrap();
        for name in EDITABLE_FILES {
            fs::write(source.join(name), "").unwrap();
        }
        fs::write(source.join("index.html"), "x").unwrap();
        // index.html (1 byte) + exactly MAX_TOTAL_BYTES of assets = total + 1.
        for i in 0..2 {
            File::create(source.join(format!("asset{i}.png")))
                .unwrap()
                .set_len(MAX_TOTAL_BYTES / 2)
                .unwrap();
        }
        // Every file meets its own limit; only the aggregate limit is exceeded.
        assert!(f.store.duplicate(&t, "copy").is_err());
        assert!(!f.store.custom_root().join("copy").exists());
        f.staging_empty();
        assert_eq!(fs::read_to_string(source.join("index.html")).unwrap(), "x");
    }

    #[test]
    fn write_atomically_replaces_file() {
        let f = Fixture::new();
        let t = f.template(true, "foo");
        let dir = f.store.resolve(&t).unwrap();
        let mut original = File::open(dir.join("app.js")).unwrap();
        f.store.write(&t, "app.js", "replacement").unwrap();
        let mut old_content = String::new();
        original.read_to_string(&mut old_content).unwrap();
        assert_eq!(old_content, "original app.js");
        assert_eq!(f.store.read(&t, "app.js").unwrap(), "replacement");
        assert_eq!(fs::read_dir(dir).unwrap().count(), EDITABLE_FILES.len());
    }

    #[test]
    #[cfg(windows)]
    fn cleanup_stale_staging_unlinks_junction_without_touching_target() {
        let f = Fixture::new();
        f.template(true, "keep");
        let _stage = f.store.create_staging().unwrap();
        let junction = f.store.staging_root().join("junction");
        let target = f.store.custom_root().join("keep");
        let status = std::process::Command::new("cmd.exe")
            .args(["/c", "mklink", "/J"])
            .arg(junction.to_string_lossy().replace('/', "\\"))
            .arg(target.to_string_lossy().replace('/', "\\"))
            .status()
            .unwrap();
        assert!(status.success());
        f.store.cleanup_stale_staging().unwrap();
        f.staging_empty();
        assert_eq!(
            fs::read_to_string(target.join("app.js")).unwrap(),
            "original app.js"
        );
    }

    #[test]
    fn template_ref_parse_roundtrip() {
        assert_eq!(
            TemplateRef::parse("default").unwrap(),
            TemplateRef::Builtin("default".into())
        );
        assert_eq!(
            TemplateRef::parse("custom:foo").unwrap(),
            TemplateRef::Custom("foo".into())
        );
        for id in ["default", "custom:foo", "custom:ABC", "my-template_1"] {
            assert_eq!(TemplateRef::parse(id).unwrap().id(), id);
        }
        for id in [
            "custom:",
            "custom:../x",
            "custom:a:b",
            "other:foo",
            "",
            "a/b",
        ] {
            assert!(TemplateRef::parse(id).is_err(), "{id}");
        }
    }

    #[test]
    fn template_ref_url_base() {
        assert_eq!(
            TemplateRef::parse("default").unwrap().url_base(),
            "/default/"
        );
        assert_eq!(
            TemplateRef::parse("custom:foo").unwrap().url_base(),
            "/custom/foo/"
        );
    }

    #[test]
    fn list_merges_both_roots_with_kind() {
        let f = Fixture::new();
        f.template(false, "default");
        f.template(true, "default");
        let list = f.store.list().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(
            list[0],
            TemplateInfo {
                id: "default".into(),
                name: "default".into(),
                kind: "builtin".into(),
                editable: false
            }
        );
        assert_eq!(
            list[1],
            TemplateInfo {
                id: "custom:default".into(),
                name: "default".into(),
                kind: "custom".into(),
                editable: true
            }
        );
        f.template(false, "aaa");
        f.template(true, "aaa");
        fs::create_dir(f.store.custom_root().join("incomplete")).unwrap();
        let ids: Vec<_> = f.store.list().unwrap().into_iter().map(|t| t.id).collect();
        assert_eq!(ids, ["aaa", "default", "custom:aaa", "custom:default"]);
    }

    #[test]
    fn identical_roots_disable_custom() {
        let f = Fixture::new();
        let builtin = f.template(false, "default");
        let store = TemplateStore::new(
            f.store.builtin_root().into(),
            f.store.builtin_root().join("../builtin"),
        );
        assert!(!store.custom_enabled());
        assert_eq!(store.list().unwrap().len(), 1);
        assert!(store.duplicate(&builtin, "copy").is_err());
        let custom = TemplateRef::Custom("default".into());
        assert!(store.delete(&custom).is_err());
        assert!(store.write(&custom, "app.js", "changed").is_err());
        assert!(store.create_staging().is_err());
        assert_eq!(
            fs::read_to_string(f.store.builtin_root().join("default/app.js")).unwrap(),
            "original app.js"
        );
    }

    #[test]
    fn write_rejects_builtin() {
        let f = Fixture::new();
        let t = f.template(false, "default");
        let err = f.store.write(&t, "style.css", "change").unwrap_err();
        assert!(err.contains("読み取り専用") && err.contains("複製"));
        assert_eq!(f.store.read(&t, "style.css").unwrap(), "original style.css");
    }

    #[test]
    fn write_custom_ok() {
        let f = Fixture::new();
        let t = f.template(true, "foo");
        for file in EDITABLE_FILES {
            f.store.write(&t, file, "/* 更新 */").unwrap();
            assert_eq!(f.store.read(&t, file).unwrap(), "/* 更新 */");
        }
        f.store
            .write(&t, "app.js", &"x".repeat(MAX_EDITABLE_BYTES as usize))
            .unwrap();
    }

    #[test]
    fn write_rejects_non_editable_file() {
        let f = Fixture::new();
        let t = f.template(true, "foo");
        for file in [
            "other.js",
            "../app.js",
            "sub/app.js",
            "fc-template.json",
            "/app.js",
            "APP.JS",
        ] {
            assert!(f.store.write(&t, file, "change").is_err(), "{file}");
        }
    }

    #[test]
    fn write_rejects_over_1mib() {
        let f = Fixture::new();
        let t = f.template(true, "foo");
        assert!(f
            .store
            .write(&t, "app.js", &"x".repeat(MAX_EDITABLE_BYTES as usize + 1))
            .is_err());
        assert_eq!(f.store.read(&t, "app.js").unwrap(), "original app.js");
    }

    #[test]
    fn duplicate_copies_allowed_files_only() {
        let f = Fixture::new();
        let t = f.template(false, "default");
        let source = f.store.resolve(&t).unwrap();
        for name in [
            "app.test.mjs",
            "evil.exe",
            "LICENSE",
            ".env",
            "con.png",
            "fc-template.json",
        ] {
            fs::write(source.join(name), "excluded").unwrap();
        }
        fs::create_dir(source.join("assets")).unwrap();
        for name in ["image.png", "font.woff2", "sound.mp3", "info.json"] {
            fs::write(source.join("assets").join(name), "asset").unwrap();
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(source.join("app.js"), source.join("link.js")).unwrap();
            std::os::unix::fs::symlink(source.join("assets"), source.join("links")).unwrap();
        }
        let info = f.store.duplicate(&t, "copy").unwrap();
        assert_eq!(info.id, "custom:copy");
        let dest = f.store.custom_root().join("copy");
        for name in [
            "app.test.mjs",
            "evil.exe",
            "LICENSE",
            ".env",
            "con.png",
            "link.js",
            "links",
        ] {
            assert!(!dest.join(name).exists(), "{name}");
        }
        for name in ["image.png", "font.woff2", "sound.mp3", "info.json"] {
            assert_eq!(
                fs::read_to_string(dest.join("assets").join(name)).unwrap(),
                "asset"
            );
        }
        assert_eq!(
            fs::read_to_string(dest.join("index.html")).unwrap(),
            "original index.html"
        );
        let metadata: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dest.join("fc-template.json")).unwrap())
                .unwrap();
        assert_eq!(metadata["schema"], 1);
        assert_eq!(metadata["origin"], "duplicate");
        assert_eq!(metadata["source"], "default");
        assert_eq!(metadata["appVersion"], env!("CARGO_PKG_VERSION"));
        assert!(metadata["createdAtMs"].as_u64().unwrap() > 0);
        // Duplicating a custom template produces fresh provenance too.
        f.store
            .duplicate(&TemplateRef::Custom("copy".into()), "copy-again")
            .unwrap();
        f.staging_empty();
    }

    #[test]
    fn duplicate_rejects_existing_and_invalid_names() {
        let f = Fixture::new();
        let t = f.template(false, "default");
        f.template(true, "Existing");
        for name in [
            "Upper",
            &"a".repeat(65),
            "-first",
            "_first",
            "con",
            "nul",
            "com1",
            "prn",
            "aux",
            "lpt9",
            "existing",
            "",
            "../escape",
            "custom:foo",
        ] {
            assert!(f.store.duplicate(&t, name).is_err(), "{name}");
        }
        // Builtin names are permitted in the custom namespace.
        f.store.duplicate(&t, "default").unwrap();
        assert!(f.store.duplicate(&t, "default").is_err());
        assert_eq!(
            f.store
                .read(&TemplateRef::Custom("Existing".into()), "app.js")
                .unwrap(),
            "original app.js"
        );
    }

    #[test]
    fn duplicate_failure_leaves_no_residue() {
        let f = Fixture::new();
        let t = f.template(false, "default");
        let source = f.store.resolve(&t).unwrap();
        // Each asset is individually allowed; their actual sum exceeds 40MiB.
        for i in 0..6 {
            File::create(source.join(format!("large{i}.png")))
                .unwrap()
                .set_len(MAX_FILE_BYTES)
                .unwrap();
        }
        assert!(f.store.duplicate(&t, "copy").is_err());
        assert!(!f.store.custom_root().join("copy").exists());
        f.staging_empty();
        assert!(source.join("index.html").exists());
    }

    #[test]
    fn delete_rejects_builtin() {
        let f = Fixture::new();
        let t = f.template(false, "default");
        assert!(f.store.delete(&t).is_err());
        assert!(f.store.resolve(&t).unwrap().join("index.html").exists());
    }

    #[test]
    fn trophy_template_is_builtin_readonly() {
        let root = std::env::temp_dir()
            .join(format!("fast-comment-trophy-{}", uuid::Uuid::new_v4()));
        let builtin = Path::new(env!("CARGO_MANIFEST_DIR")).join("../templates");
        let store = TemplateStore::new(builtin.clone(), root.join("config/templates"));
        let t = TemplateRef::parse("trophy").unwrap();
        assert!(matches!(t, TemplateRef::Builtin(_)));
        assert!(builtin.join("trophy/index.html").is_file());
        let before = store.read(&t, "style.css").ok();
        let err = store.write(&t, "style.css", "change").unwrap_err();
        assert!(err.contains("読み取り専用") && err.contains("複製"));
        assert!(store.delete(&t).is_err());
        assert!(builtin.join("trophy/index.html").is_file());
        assert_eq!(store.read(&t, "style.css").ok(), before);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn effects_template_is_builtin_readonly_and_listed() {
        let root = std::env::temp_dir()
            .join(format!("fast-comment-effects-{}", uuid::Uuid::new_v4()));
        let builtin = Path::new(env!("CARGO_MANIFEST_DIR")).join("../templates");
        let store = TemplateStore::new(builtin.clone(), root.join("config/templates"));
        let t = TemplateRef::parse("effects").unwrap();
        assert!(matches!(t, TemplateRef::Builtin(_)));
        for file in ["index.html", "app.js", "style.css"] {
            assert!(builtin.join("effects").join(file).is_file(), "{file}");
        }
        assert!(store.list().unwrap().iter().any(|info| info.name == "effects"));
        let err = store.write(&t, "style.css", "change").unwrap_err();
        assert!(err.contains("読み取り専用"));
        assert!(store.delete(&t).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn delete_removes_custom() {
        let f = Fixture::new();
        let t = f.template(true, "foo");
        f.template(false, "foo");
        f.store.delete(&t).unwrap();
        assert!(!f.store.custom_root().join("foo").exists());
        assert!(f.store.builtin_root().join("foo/index.html").exists());
    }

    #[test]
    fn delete_rejects_missing() {
        let f = Fixture::new();
        assert!(f
            .store
            .delete(&TemplateRef::Custom("missing".into()))
            .is_err());
    }

    #[test]
    #[cfg(unix)]
    fn delete_rejects_symlink() {
        let f = Fixture::new();
        f.template(false, "default");
        std::os::unix::fs::symlink(
            f.store.builtin_root().join("default"),
            f.store.custom_root().join("link"),
        )
        .unwrap();
        assert!(f.store.delete(&TemplateRef::Custom("link".into())).is_err());
        assert!(f.store.builtin_root().join("default/index.html").exists());
    }

    #[test]
    fn commit_staged_is_all_or_nothing() {
        let f = Fixture::new();
        let stage = f.store.create_staging().unwrap();
        assert_eq!(
            stage.path().parent().unwrap(),
            f.root.join("config/templates-staging")
        );
        assert!(!stage.path().starts_with(f.store.custom_root()));
        fs::write(stage.path().join("index.html"), "complete").unwrap();
        fs::write(stage.path().join("app.js"), "complete-js").unwrap();
        assert!(f.store.list().unwrap().is_empty());
        let old_stage = stage.path().to_path_buf();
        f.store.commit_staged(stage, "published").unwrap();
        assert!(!old_stage.exists());
        assert_eq!(
            fs::read_to_string(f.store.custom_root().join("published/app.js")).unwrap(),
            "complete-js"
        );
        let collision = f.store.create_staging().unwrap();
        fs::write(collision.path().join("index.html"), "replacement").unwrap();
        assert!(f.store.commit_staged(collision, "published").is_err());
        assert_eq!(
            fs::read_to_string(f.store.custom_root().join("published/index.html")).unwrap(),
            "complete"
        );
        let incomplete = f.store.create_staging().unwrap();
        fs::write(incomplete.path().join("app.js"), "incomplete").unwrap();
        assert!(f.store.commit_staged(incomplete, "incomplete").is_err());
        assert!(!f.store.custom_root().join("incomplete").exists());
        f.staging_empty();
    }

    #[test]
    fn cleanup_stale_staging_removes_only_staging() {
        let f = Fixture::new();
        f.template(true, "keep");
        f.template(false, "keep");
        let stage = f.store.create_staging().unwrap();
        fs::write(stage.path().join("partial.js"), "partial").unwrap();
        fs::write(f.store.staging_root().join("partial-file"), "partial").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            f.store.custom_root().join("keep"),
            f.store.staging_root().join("link"),
        )
        .unwrap();
        f.store.cleanup_stale_staging().unwrap();
        f.staging_empty();
        assert!(f.store.custom_root().join("keep/index.html").exists());
        assert!(f.store.builtin_root().join("keep/index.html").exists());
        // Repeated startup cleanup is harmless.
        f.store.cleanup_stale_staging().unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn aliases_and_symlink_files_are_guarded() {
        let f = Fixture::new();
        let builtin = f.template(false, "default");
        let alias = f.root.join("alias");
        std::os::unix::fs::symlink(f.store.builtin_root(), &alias).unwrap();
        let same = TemplateStore::new(f.store.builtin_root().into(), alias);
        assert!(!same.custom_enabled());
        assert!(same.duplicate(&builtin, "copy").is_err());
        let t = f.template(true, "foo");
        let app = f.store.custom_root().join("foo/app.js");
        fs::remove_file(&app).unwrap();
        std::os::unix::fs::symlink(f.store.builtin_root().join("default/app.js"), app).unwrap();
        assert!(f.store.write(&t, "app.js", "changed").is_err());
        assert!(f.store.read(&t, "app.js").is_err());
        assert_eq!(f.store.read(&builtin, "app.js").unwrap(), "original app.js");
        fs::create_dir_all(f.root.join("config")).unwrap();
        std::os::unix::fs::symlink(f.store.custom_root(), f.store.staging_root()).unwrap();
        assert!(f.store.create_staging().is_err());
        assert!(f.store.cleanup_stale_staging().is_err());
        assert!(f.store.custom_root().join("foo/index.html").exists());
    }

    #[test]
    fn asset_policy_rejects_unsafe_paths() {
        for name in [
            "../x.js",
            "/x.js",
            "a\\b.js",
            "C:x.js",
            ".env",
            "con.png",
            "a./x.js",
            "a/b/c/d/e.png",
            "app.test.mjs",
            "日本語.png",
            "__MACOSX/.DS_Store",
        ] {
            assert!(!is_allowed_asset(name), "{name}");
        }
        for name in ["app.js", "assets/image.png", "a/b/c/d.svg", "readme.md"] {
            assert!(is_allowed_asset(name), "{name}");
        }
    }
}
