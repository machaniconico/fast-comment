//! Validation-first import of custom OBS templates from a zip archive.

use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{Cursor, Read};

use zip::ZipArchive;

use crate::templates::{is_allowed_asset, TemplateInfo, TemplateStore, EDITABLE_FILES};

pub const MAX_ZIP_BYTES: u64 = 100 * 1024 * 1024;
pub const MAX_TOTAL_BYTES: u64 = 100 * 1024 * 1024;
pub const MAX_FILE_BYTES: u64 = 100 * 1024 * 1024;
pub const MAX_EDITABLE_BYTES: u64 = 1024 * 1024;
pub const MAX_ENTRIES: usize = 500;
pub const MAX_DEPTH: usize = 4;
/// Central directory records beyond this are rejected before any per-entry work.
const MAX_CENTRAL_ENTRIES: usize = 5000;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ImportResult {
    pub template: TemplateInfo,
    pub files: usize,
    pub skipped: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
struct Limits {
    zip_bytes: u64,
    total_bytes: u64,
    file_bytes: u64,
    editable_bytes: u64,
    entries: usize,
    depth: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            zip_bytes: MAX_ZIP_BYTES,
            total_bytes: MAX_TOTAL_BYTES,
            file_bytes: MAX_FILE_BYTES,
            editable_bytes: MAX_EDITABLE_BYTES,
            entries: MAX_ENTRIES,
            depth: MAX_DEPTH,
        }
    }
}

struct EntryName {
    components: Vec<String>,
    directory: bool,
}

struct Candidate {
    index: usize,
    original: String,
    components: Vec<String>,
}

fn size_label(bytes: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    if bytes >= MIB && bytes % MIB == 0 {
        format!("{}MiB", bytes / MIB)
    } else if bytes >= 1024 && bytes % 1024 == 0 {
        format!("{}KiB", bytes / 1024)
    } else {
        format!("{bytes} バイト")
    }
}

fn path_error(name: &str) -> String {
    format!("zip 内のパスが不正です（{name}）")
}

fn parse_entry_name(raw: &str) -> Result<EntryName, String> {
    if raw.is_empty() || raw.contains('\0') {
        return Err(path_error(raw));
    }
    if raw.contains('\\') || raw.contains(':') || raw.starts_with('/') {
        return Err(path_error(raw));
    }
    let directory = raw.ends_with('/');
    let trimmed = if directory {
        &raw[..raw.len() - 1]
    } else {
        raw
    };
    if trimmed.is_empty() {
        return Err(path_error(raw));
    }
    let mut components = Vec::new();
    for part in trimmed.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return Err(path_error(raw));
        }
        components.push(part.to_string());
    }
    Ok(EntryName {
        components,
        directory,
    })
}

fn is_os_junk(components: &[String]) -> bool {
    if components
        .first()
        .is_some_and(|part| part.eq_ignore_ascii_case("__MACOSX"))
    {
        return true;
    }
    let Some(name) = components.last() else {
        return false;
    };
    [".ds_store", "thumbs.db", "desktop.ini", "fc-template.json"]
        .iter()
        .any(|junk| name.eq_ignore_ascii_case(junk))
}

fn check_entry_kind(entry: &zip::read::ZipFile<'_, impl Read>, name: &str) -> Result<(), String> {
    if entry.is_symlink() {
        return Err(format!(
            "zip 内にシンボリックリンクが含まれています（{name}）"
        ));
    }
    if let Some(mode) = entry.unix_mode() {
        let kind = mode & 0o170000;
        if kind != 0 && kind != 0o100000 && kind != 0o040000 {
            return Err(format!(
                "通常ファイル以外のエントリは取り込めません（{name}）"
            ));
        }
    }
    Ok(())
}

/// Whether every entry sits under one top folder. Top folder names that differ only by case are rejected.
fn strip_single_root(candidates: &[Candidate]) -> Result<bool, String> {
    let Some(first) = candidates.first() else {
        return Ok(false);
    };
    let Some(root) = first.components.first() else {
        return Ok(false);
    };
    let single = candidates.iter().all(|candidate| {
        candidate.components.len() >= 2
            && candidate
                .components
                .first()
                .is_some_and(|part| part.eq_ignore_ascii_case(root))
    });
    if single {
        if let Some(other) = candidates
            .iter()
            .find(|candidate| candidate.components.first() != Some(root))
        {
            return Err(duplicate_error(&other.original));
        }
    }
    Ok(single)
}

fn duplicate_error(original: &str) -> String {
    format!("大文字小文字を無視すると重複するパスがあります（{original}）")
}

/// Registers a file path and its parent folders; names that differ only by case collide.
fn register_path(
    seen: &mut HashSet<String>,
    dirs: &mut HashMap<String, String>,
    components: &[String],
    original: &str,
) -> Result<(), String> {
    for end in 1..components.len() {
        let prefix = components[..end].join("/");
        let key = prefix.to_ascii_lowercase();
        if seen.contains(&key) {
            return Err(duplicate_error(original));
        }
        match dirs.get(&key) {
            Some(previous) if *previous != prefix => return Err(duplicate_error(original)),
            Some(_) => {}
            None => {
                dirs.insert(key, prefix);
            }
        }
    }
    let key = components.join("/").to_ascii_lowercase();
    if dirs.contains_key(&key) || !seen.insert(key) {
        return Err(duplicate_error(original));
    }
    Ok(())
}

/// Imports a zip archive as a new `custom:<name>` template.
pub fn import_zip(store: &TemplateStore, name: &str, bytes: &[u8]) -> Result<ImportResult, String> {
    import_zip_with_limits(store, name, bytes, Limits::default())
}

fn import_zip_with_limits(
    store: &TemplateStore,
    name: &str,
    bytes: &[u8],
    limits: Limits,
) -> Result<ImportResult, String> {
    if bytes.len() as u64 > limits.zip_bytes {
        return Err(format!(
            "zip ファイルが大きすぎます（上限 {}）",
            size_label(limits.zip_bytes)
        ));
    }
    store.validate_new_name(name)?;

    let mut archive =
        ZipArchive::new(Cursor::new(bytes)).map_err(|e| format!("zip を読み込めません: {e}"))?;
    // Same-name entries are merged by the name map, which would hide them from the count and duplicate checks.
    if archive.has_duplicate_names() {
        return Err("zip 内に同じ名前のエントリが複数あります".into());
    }
    if archive.len() > MAX_CENTRAL_ENTRIES.max(limits.entries) {
        return Err(format!(
            "zip 内のエントリ数が多すぎます（上限 {} 件）",
            limits.entries
        ));
    }

    let mut candidates = Vec::new();
    for index in 0..archive.len() {
        // Raw access: never decrypt or decompress while only inspecting headers.
        let entry = archive
            .by_index_raw(index)
            .map_err(|e| format!("zip を読み込めません: {e}"))?;
        let original = entry
            .name()
            .map_err(|e| format!("zip のファイル名を読み取れません: {e}"))?
            .into_owned();
        if entry.encrypted() {
            return Err(format!("暗号化された zip は取り込めません（{original}）"));
        }
        check_entry_kind(&entry, &original)?;
        let non_utf8 = std::str::from_utf8(entry.name_raw()).is_err();
        let parsed = parse_entry_name(&original).map_err(|e| {
            if non_utf8 && original.contains('\\') {
                format!("{e}。ファイル名が UTF-8 でない zip の可能性があります（Windows 標準の圧縮で日本語名を含む等）。UTF-8 で圧縮し直してください")
            } else {
                e
            }
        })?;
        if parsed.directory || is_os_junk(&parsed.components) {
            continue;
        }
        if candidates.len() >= limits.entries {
            return Err(format!(
                "zip 内のエントリ数が多すぎます（上限 {} 件）",
                limits.entries
            ));
        }
        candidates.push(Candidate {
            index,
            original,
            components: parsed.components,
        });
    }

    if candidates.is_empty() {
        return Err("ルートに index.html がありません".into());
    }

    let strip_root = strip_single_root(&candidates)?;
    let mut seen = HashSet::new();
    let mut dirs = HashMap::new();
    let mut planned = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let mut components = if strip_root {
            candidate.components[1..].to_vec()
        } else {
            candidate.components.clone()
        };
        if components.is_empty() {
            return Err(path_error(&candidate.original));
        }
        if components.len() > limits.depth {
            return Err(format!(
                "zip 内の階層が深すぎます（{}、上限 {} 階層）",
                candidate.original, limits.depth
            ));
        }
        // Windows resolves names case-insensitively: write editable files under their canonical name.
        if let [only] = components.as_mut_slice() {
            if let Some(canonical) = EDITABLE_FILES
                .iter()
                .find(|file| file.eq_ignore_ascii_case(only))
            {
                *only = (*canonical).to_string();
            }
        }
        let relative = components.join("/");
        register_path(&mut seen, &mut dirs, &components, &candidate.original)?;
        let allowed = is_allowed_asset(&relative);
        planned.push((candidate, relative, allowed));
    }

    if !planned
        .iter()
        .any(|(_, relative, allowed)| *allowed && relative == "index.html")
    {
        return Err("ルートに index.html がありません".into());
    }

    let staged = store.create_staging()?;
    let mut files = 0;
    let mut skipped = Vec::new();
    let mut total: u64 = 0;

    for (candidate, relative, allowed) in planned {
        if !allowed {
            skipped.push(candidate.original);
            continue;
        }

        let editable = !relative.contains('/') && EDITABLE_FILES.contains(&relative.as_str());
        let per_file = if editable {
            limits.editable_bytes
        } else {
            limits.file_bytes
        };
        let remaining = limits.total_bytes.saturating_sub(total);
        let cap = per_file.min(remaining);

        let entry = archive
            .by_index(candidate.index)
            .map_err(|e| format!("zip を読み込めません: {e}"))?;
        let mut input = entry.take(cap.saturating_add(1));

        let path = staged.path().join(&relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                format!("テンプレートの展開先を作成できません（{relative}）: {e}")
            })?;
        }
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| format!("テンプレートのファイルを作成できません（{relative}）: {e}"))?;
        let written = std::io::copy(&mut input, &mut output)
            .map_err(|e| format!("zip を展開できません（{}）: {e}", candidate.original))?;

        if written > per_file {
            let label = if editable {
                "編集対象ファイルが大きすぎます"
            } else {
                "ファイルが大きすぎます"
            };
            return Err(format!(
                "{label}（{}、上限 {}）",
                candidate.original,
                size_label(per_file)
            ));
        }
        if written > remaining {
            return Err(format!(
                "展開後の合計サイズが上限を超えています（上限 {}）",
                size_label(limits.total_bytes)
            ));
        }

        total += written;
        files += 1;
    }

    store.write_provenance(&staged, "zip", name)?;
    let template = store.commit_staged(staged, name)?;
    Ok(ImportResult {
        template,
        files,
        skipped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    use std::path::PathBuf;
    use zip::write::SimpleFileOptions;
    use zip::{CompressionMethod, ZipWriter};

    struct Fixture {
        root: PathBuf,
        store: TemplateStore,
    }

    impl Fixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("fast-comment-import-{}", uuid::Uuid::new_v4()));
            let store = TemplateStore::new(root.join("builtin"), root.join("config/templates"));
            fs::create_dir_all(store.builtin_root()).unwrap();
            fs::create_dir_all(store.custom_root()).unwrap();
            Self { root, store }
        }

        fn staging_empty(&self) {
            assert!(fs::read_dir(self.store.staging_root())
                .map(|mut e| e.next().is_none())
                .unwrap_or(true));
        }

        fn imported(&self, name: &str) -> PathBuf {
            self.store.custom_root().join(name)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn zip_entries(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for (name, data) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(data).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn snapshot(dir: &std::path::Path) -> Vec<String> {
        fn walk(dir: &std::path::Path, out: &mut Vec<String>) {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    out.push(entry.path().to_string_lossy().into_owned());
                    if entry.path().is_dir() {
                        walk(&entry.path(), out);
                    }
                }
            }
        }
        let mut out = Vec::new();
        walk(dir, &mut out);
        out.sort();
        out
    }

    fn dir_names(dir: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// Overwrites the uncompressed-size field of `name` in its local header and
    /// central directory record.
    fn spoof_uncompressed_size(bytes: &mut [u8], name: &str, fake: u32) {
        let needle = name.as_bytes();
        let mut patched = 0;
        for pos in 0..bytes.len().saturating_sub(needle.len()) {
            if &bytes[pos..pos + needle.len()] != needle {
                continue;
            }
            if pos >= 30 && bytes[pos - 30..pos - 26] == [0x50, 0x4b, 0x03, 0x04] {
                bytes[pos - 8..pos - 4].copy_from_slice(&fake.to_le_bytes());
                patched += 1;
            } else if pos >= 46 && bytes[pos - 46..pos - 42] == [0x50, 0x4b, 0x01, 0x02] {
                bytes[pos - 22..pos - 18].copy_from_slice(&fake.to_le_bytes());
                patched += 1;
            }
        }
        assert_eq!(patched, 2, "local + central header of {name}");
    }

    fn files_zip(count: usize, junk: usize, dirs: usize) -> Vec<u8> {
        let options = SimpleFileOptions::default();
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        writer.start_file("index.html", options).unwrap();
        writer.write_all(b"index").unwrap();
        for i in 1..count {
            writer.start_file(format!("file{i}.png"), options).unwrap();
            writer.write_all(b"x").unwrap();
        }
        for i in 0..junk {
            writer.start_file(format!("__MACOSX/j{i}"), options).unwrap();
            writer.write_all(b"x").unwrap();
        }
        for i in 0..dirs {
            writer.add_directory(format!("dir{i}/"), options).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn limits_match_contract() {
        assert_eq!(MAX_ZIP_BYTES, 100 * 1024 * 1024);
        assert_eq!(MAX_TOTAL_BYTES, 100 * 1024 * 1024);
        assert_eq!(MAX_FILE_BYTES, 100 * 1024 * 1024);
        assert_eq!(MAX_EDITABLE_BYTES, 1024 * 1024);
        assert_eq!(MAX_ENTRIES, 500);
        assert_eq!(MAX_DEPTH, 4);
    }

    #[test]
    fn import_ok_flat_deflate() {
        let f = Fixture::new();
        let bytes = zip_entries(&[
            ("index.html", b"<html>index</html>"),
            ("style.css", b"body{}"),
            ("app.js", b"console.log(1)"),
            ("assets/pic.png", b"png"),
        ]);
        let result = import_zip(&f.store, "sample", &bytes).unwrap();
        assert_eq!(result.template.id, "custom:sample");
        assert_eq!(result.files, 4);
        assert!(result.skipped.is_empty());
        let dir = f.imported("sample");
        assert_eq!(
            fs::read_to_string(dir.join("index.html")).unwrap(),
            "<html>index</html>"
        );
        assert_eq!(
            fs::read_to_string(dir.join("assets/pic.png")).unwrap(),
            "png"
        );
        let metadata: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join("fc-template.json")).unwrap())
                .unwrap();
        assert_eq!(metadata["origin"], "zip");
        assert_eq!(metadata["source"], "sample");
        assert_eq!(metadata["appVersion"], env!("CARGO_PKG_VERSION"));
        f.staging_empty();
    }

    #[test]
    fn import_ok_strips_single_top_dir() {
        let f = Fixture::new();
        let bytes = zip_entries(&[
            ("my-template/index.html", b"index"),
            ("my-template/style.css", b"css"),
            ("my-template/app.js", b"js"),
        ]);
        let result = import_zip(&f.store, "stripped", &bytes).unwrap();
        assert_eq!(result.template.id, "custom:stripped");
        assert_eq!(result.files, 3);
        let dir = f.imported("stripped");
        assert_eq!(fs::read_to_string(dir.join("index.html")).unwrap(), "index");
        assert!(!dir.join("my-template").exists());
        f.staging_empty();
    }

    #[test]
    fn rejects_zip_slip() {
        let f = Fixture::new();
        let before = snapshot(&f.root);
        for evil in [
            "../evil.js",
            "a/../../evil.js",
            "/abs.js",
            "C:/x.js",
            "a\\b.js",
            "x.png:stream",
        ] {
            let bytes = zip_entries(&[(evil, b"evil"), ("index.html", b"index")]);
            assert!(import_zip(&f.store, "slip", &bytes).is_err(), "{evil}");
            assert!(!f.imported("slip").exists(), "{evil}");
            assert!(!f.root.join("evil.js").exists(), "{evil}");
            assert_eq!(snapshot(&f.root), before, "{evil}");
            f.staging_empty();
        }
    }

    #[test]
    fn rejects_symlink_and_encrypted_entries() {
        use zip::unstable::write::FileOptionsExt;

        let f = Fixture::new();
        let options = SimpleFileOptions::default();
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        writer.start_file("index.html", options).unwrap();
        writer.write_all(b"index").unwrap();
        writer
            .add_symlink("link.png", "index.html", options)
            .unwrap();
        let symlink_zip = writer.finish().unwrap().into_inner();
        assert!(import_zip(&f.store, "symlink", &symlink_zip).is_err());
        assert!(!f.imported("symlink").exists());
        f.staging_empty();

        let encrypted_options = options.with_deprecated_encryption(b"password").unwrap();
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        writer.start_file("index.html", options).unwrap();
        writer.write_all(b"index").unwrap();
        writer.start_file("secret.png", encrypted_options).unwrap();
        writer.write_all(b"secret").unwrap();
        let encrypted_zip = writer.finish().unwrap().into_inner();
        let err = import_zip(&f.store, "encrypted", &encrypted_zip).unwrap_err();
        assert!(err.contains("暗号化された zip は取り込めません"), "{err}");
        assert!(!f.imported("encrypted").exists());
        f.staging_empty();
    }

    #[test]
    fn rejects_case_insensitive_duplicates() {
        let f = Fixture::new();
        let bytes = zip_entries(&[("index.html", b"index"), ("A.png", b"a"), ("a.png", b"b")]);
        let err = import_zip(&f.store, "duplicate", &bytes).unwrap_err();
        assert!(err.contains("重複"));
        assert!(!f.imported("duplicate").exists());
        f.staging_empty();
    }

    #[test]
    fn rejects_missing_index_html() {
        let f = Fixture::new();
        let bytes = zip_entries(&[("style.css", b"css"), ("app.js", b"js")]);
        let err = import_zip(&f.store, "noindex", &bytes).unwrap_err();
        assert!(err.contains("index.html"));
        assert!(!f.imported("noindex").exists());
        f.staging_empty();
    }

    #[test]
    fn rejects_zip_bomb_by_actual_bytes() {
        let f = Fixture::new();

        let mut limits = Limits::default();
        limits.file_bytes = 32;
        let bytes = zip_entries(&[("index.html", b"index"), ("big.png", &[b'x'; 33])]);
        assert!(import_zip_with_limits(&f.store, "filebomb", &bytes, limits).is_err());
        assert!(!f.imported("filebomb").exists());
        f.staging_empty();

        let mut limits = Limits::default();
        limits.total_bytes = 40;
        let bytes = zip_entries(&[
            ("index.html", b"index"),
            ("a.png", &[b'a'; 20]),
            ("b.png", &[b'b'; 20]),
        ]);
        assert!(import_zip_with_limits(&f.store, "totalbomb", &bytes, limits).is_err());
        assert!(!f.imported("totalbomb").exists());
        f.staging_empty();

        // Headers claim 1 byte; the real data is 64 bytes against a 32 byte limit.
        let mut limits = Limits::default();
        limits.file_bytes = 32;
        let mut bytes = zip_entries(&[("index.html", b"index"), ("liar.png", &[b'x'; 64])]);
        spoof_uncompressed_size(&mut bytes, "liar.png", 1);
        let err = import_zip_with_limits(&f.store, "liar", &bytes, limits).unwrap_err();
        // 申告サイズ偽装は zip crate が展開時に検出する（こちらの上限文言ではない）。
        assert!(err.contains("zip を展開できません") && err.contains("liar.png"), "{err}");
        assert!(!f.imported("liar").exists());
        f.staging_empty();
    }

    #[test]
    fn rejects_too_many_entries() {
        let f = Fixture::new();
        let bytes = files_zip(MAX_ENTRIES + 1, 0, 0);
        let err = import_zip(&f.store, "many", &bytes).unwrap_err();
        assert!(err.contains("エントリ数") && err.contains("上限"), "{err}");
        assert!(!f.imported("many").exists());
        f.staging_empty();
    }

    #[test]
    fn accepts_exactly_max_entries() {
        let f = Fixture::new();
        let bytes = files_zip(MAX_ENTRIES, 0, 0);
        let result = import_zip(&f.store, "exact", &bytes).unwrap();
        assert_eq!(result.files, MAX_ENTRIES);
        f.staging_empty();
    }

    #[test]
    fn entry_count_ignores_junk_and_directories() {
        let f = Fixture::new();
        let bytes = files_zip(MAX_ENTRIES, 50, 50);
        let result = import_zip(&f.store, "counted", &bytes).unwrap();
        assert_eq!(result.files, MAX_ENTRIES);
        f.staging_empty();
    }

    #[test]
    fn rejects_huge_central_directory_early() {
        let f = Fixture::new();
        let bytes = files_zip(1, MAX_CENTRAL_ENTRIES + 1, 0);
        let err = import_zip(&f.store, "huge", &bytes).unwrap_err();
        assert!(err.contains("エントリ数"), "{err}");
        assert!(!f.imported("huge").exists());
        f.staging_empty();
    }

    #[test]
    fn rejects_oversized_archive() {
        let f = Fixture::new();
        let bytes = zip_entries(&[("index.html", b"index")]);
        let mut limits = Limits::default();
        limits.zip_bytes = 8;
        assert!(import_zip_with_limits(&f.store, "large", &bytes, limits).is_err());
        assert!(!f.imported("large").exists());
        f.staging_empty();
    }

    #[test]
    fn rejects_too_deep_path() {
        let f = Fixture::new();
        let bytes = zip_entries(&[("index.html", b"index"), ("a/b/c/d/e.png", b"deep")]);
        let err = import_zip(&f.store, "deep", &bytes).unwrap_err();
        assert!(err.contains("階層"));
        assert!(!f.imported("deep").exists());
        f.staging_empty();
    }

    #[test]
    fn rejects_oversized_editable_file() {
        let f = Fixture::new();
        let big = vec![b'x'; MAX_EDITABLE_BYTES as usize + 1];
        let bytes = zip_entries(&[("index.html", b"index"), ("app.js", &big)]);
        let err = import_zip(&f.store, "bigedit", &bytes).unwrap_err();
        assert!(err.contains("編集対象"));
        assert!(!f.imported("bigedit").exists());
        f.staging_empty();
    }

    #[test]
    fn skips_disallowed_and_reports() {
        let f = Fixture::new();
        let bytes = zip_entries(&[
            ("index.html", b"index"),
            ("readme.exe", b"exe"),
            ("LICENSE", b"license"),
            ("app.mjs", b"mjs"),
            (".env", b"env"),
            ("con.png", b"con"),
            ("日本語名.png", b"ja"),
            ("trailing.", b"dot"),
        ]);
        let result = import_zip(&f.store, "skips", &bytes).unwrap();
        assert_eq!(result.files, 1);
        for name in [
            "readme.exe",
            "LICENSE",
            "app.mjs",
            ".env",
            "con.png",
            "日本語名.png",
            "trailing.",
        ] {
            assert!(
                result.skipped.iter().any(|skipped| skipped == name),
                "{name}: {:?}",
                result.skipped
            );
        }
        assert_eq!(
            fs::read_to_string(f.imported("skips").join("index.html")).unwrap(),
            "index"
        );
        f.staging_empty();
    }

    #[test]
    fn ignores_os_junk_silently() {
        let f = Fixture::new();
        let bytes = zip_entries(&[
            ("index.html", b"index"),
            ("__MACOSX/._index.html", b"junk"),
            (".DS_Store", b"junk"),
            ("Thumbs.db", b"junk"),
            ("desktop.ini", b"junk"),
            ("fc-template.json", b"{\"origin\":\"fake\"}"),
            ("assets/Thumbs.db", b"junk"),
        ]);
        let result = import_zip(&f.store, "junk", &bytes).unwrap();
        assert!(result.skipped.is_empty(), "{:?}", result.skipped);
        let dir = f.imported("junk");
        let metadata: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join("fc-template.json")).unwrap())
                .unwrap();
        assert_eq!(metadata["origin"], "zip");
        f.staging_empty();
    }

    #[test]
    fn rejects_existing_or_invalid_name() {
        let f = Fixture::new();
        let bytes = zip_entries(&[("index.html", b"index"), ("app.js", b"original")]);
        import_zip(&f.store, "existing", &bytes).unwrap();
        let other = zip_entries(&[("index.html", b"other"), ("app.js", b"replaced")]);
        let err = import_zip(&f.store, "existing", &other).unwrap_err();
        assert!(err.contains("同名"), "{err}");
        assert_eq!(
            fs::read_to_string(f.imported("existing").join("app.js")).unwrap(),
            "original"
        );
        f.staging_empty();

        let manual = f.store.custom_root().join("Manual");
        fs::create_dir_all(&manual).unwrap();
        fs::write(manual.join("marker.txt"), "keep").unwrap();
        let err = import_zip(&f.store, "manual", &other).unwrap_err();
        assert!(err.contains("同名"), "{err}");
        assert_eq!(fs::read_to_string(manual.join("marker.txt")).unwrap(), "keep");
        assert_eq!(dir_names(f.store.custom_root()), ["Manual", "existing"]);
        f.staging_empty();

        for name in ["Existing", "bad name", "-bad", "con"] {
            assert!(import_zip(&f.store, name, &bytes).is_err(), "{name}");
        }
        f.staging_empty();
    }

    #[test]
    fn accepts_values_exactly_at_limits() {
        let f = Fixture::new();

        let mut limits = Limits::default();
        limits.file_bytes = 32;
        let bytes = zip_entries(&[("index.html", b"index"), ("big.png", &[b'x'; 32])]);
        import_zip_with_limits(&f.store, "fileedge", &bytes, limits).unwrap();

        let mut limits = Limits::default();
        limits.total_bytes = 40;
        let bytes = zip_entries(&[
            ("index.html", b"index"),
            ("a.png", &[b'a'; 20]),
            ("b.png", &[b'b'; 15]),
        ]);
        import_zip_with_limits(&f.store, "totaledge", &bytes, limits).unwrap();

        let js = vec![b'x'; MAX_EDITABLE_BYTES as usize];
        let bytes = zip_entries(&[("index.html", b"index"), ("app.js", &js)]);
        import_zip(&f.store, "editedge", &bytes).unwrap();

        let bytes = zip_entries(&[("index.html", b"index"), ("a/b/c/d.png", b"png")]);
        let mut limits = Limits::default();
        limits.depth = 4;
        import_zip_with_limits(&f.store, "depthedge", &bytes, limits).unwrap();
        f.staging_empty();
    }

    #[test]
    fn editable_names_match_case_insensitively() {
        let f = Fixture::new();
        let big = vec![b'x'; 2 * 1024 * 1024];
        let bytes = zip_entries(&[("index.html", b"index"), ("APP.JS", &big)]);
        let err = import_zip(&f.store, "bigupper", &bytes).unwrap_err();
        assert!(err.contains("編集対象"), "{err}");
        assert!(!f.imported("bigupper").exists());
        f.staging_empty();

        let bytes = zip_entries(&[
            ("INDEX.HTML", b"upper"),
            ("Style.CSS", b"css"),
            ("assets/APP.JS", b"nested"),
        ]);
        let result = import_zip(&f.store, "upper", &bytes).unwrap();
        assert_eq!(result.files, 3);
        let dir = f.imported("upper");
        assert_eq!(
            dir_names(&dir),
            ["assets", "fc-template.json", "index.html", "style.css"]
        );
        assert_eq!(fs::read_to_string(dir.join("index.html")).unwrap(), "upper");
        f.staging_empty();
    }

    #[test]
    fn rejects_symlink_even_with_junk_name() {
        let f = Fixture::new();
        let options = SimpleFileOptions::default();
        for link in ["__MACOSX/link", ".DS_Store"] {
            let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
            writer.start_file("index.html", options).unwrap();
            writer.write_all(b"index").unwrap();
            writer.add_symlink(link, "index.html", options).unwrap();
            let bytes = writer.finish().unwrap().into_inner();
            let err = import_zip(&f.store, "junklink", &bytes).unwrap_err();
            assert!(err.contains("シンボリックリンク"), "{link}: {err}");
            assert!(!f.imported("junklink").exists());
            f.staging_empty();
        }
    }

    #[test]
    fn errors_do_not_leak_staging_path() {
        let f = Fixture::new();
        let root = f.root.to_string_lossy().into_owned();
        let staging = f.store.staging_root().to_string_lossy().into_owned();
        let mut limits = Limits::default();
        limits.file_bytes = 32;
        limits.total_bytes = 40;
        let cases = [
            zip_entries(&[("index.html", b"index"), ("big.png", &[b'x'; 33])]),
            zip_entries(&[
                ("index.html", b"index"),
                ("a.png", &[b'a'; 20]),
                ("b.png", &[b'b'; 20]),
            ]),
            zip_entries(&[("index.html", b"index"), ("a/b/c/d/e.png", b"deep")]),
            zip_entries(&[("index.html", b"index"), ("../x.png", b"slip")]),
            zip_entries(&[("index.html", b"index"), ("a.png", b"1"), ("A.png", b"2")]),
        ];
        for (i, bytes) in cases.iter().enumerate() {
            let err = import_zip_with_limits(&f.store, "leak", bytes, limits).unwrap_err();
            assert!(!err.contains(&root) && !err.contains(&staging), "{i}: {err}");
            f.staging_empty();
        }
    }

    #[test]
    fn rejects_folder_names_differing_only_by_case() {
        let f = Fixture::new();
        let bytes = zip_entries(&[
            ("index.html", b"index"),
            ("Assets/a.png", b"a"),
            ("assets/b.png", b"b"),
        ]);
        let err = import_zip(&f.store, "folders", &bytes).unwrap_err();
        assert!(err.contains("重複"), "{err}");
        assert!(!f.imported("folders").exists());
        f.staging_empty();

        // Same-case siblings in one folder are fine.
        let bytes = zip_entries(&[
            ("index.html", b"index"),
            ("assets/a.png", b"a"),
            ("assets/b.png", b"b"),
        ]);
        import_zip(&f.store, "siblings", &bytes).unwrap();
    }

    #[test]
    fn rejects_top_folder_names_differing_only_by_case() {
        let f = Fixture::new();
        let bytes = zip_entries(&[("Theme/index.html", b"index"), ("theme/app.js", b"js")]);
        let err = import_zip(&f.store, "topcase", &bytes).unwrap_err();
        assert!(err.contains("重複"), "{err}");
        assert!(!f.imported("topcase").exists());
        f.staging_empty();

        // A single consistently-cased top folder is still stripped.
        let bytes = zip_entries(&[("Theme/index.html", b"index"), ("Theme/app.js", b"js")]);
        import_zip(&f.store, "topok", &bytes).unwrap();
    }

    #[test]
    fn rejects_exact_duplicate_entry_names() {
        let f = Fixture::new();
        // ZipWriter refuses duplicates, so write distinct same-length names and rename in place.
        let mut bytes = zip_entries(&[("index.htmX", b"a"), ("index.htmY", b"b")]);
        for (from, to) in [(&b"index.htmX"[..], &b"index.html"[..]), (&b"index.htmY"[..], &b"index.html"[..])] {
            let mut i = 0;
            while i + from.len() <= bytes.len() {
                if &bytes[i..i + from.len()] == from {
                    bytes[i..i + from.len()].copy_from_slice(to);
                    i += from.len();
                } else {
                    i += 1;
                }
            }
        }
        let archive = ZipArchive::new(Cursor::new(&bytes[..])).unwrap();
        assert!(archive.has_duplicate_names());
        let err = import_zip(&f.store, "dupname", &bytes).unwrap_err();
        assert!(err.contains("同じ名前"), "{err}");
        assert!(!f.imported("dupname").exists());
        f.staging_empty();
    }

    #[test]
    fn hints_non_utf8_names_with_backslash() {
        let f = Fixture::new();
        let mut bytes = zip_entries(&[("index.html", b"index"), ("AB\\a.png", b"x")]);
        let needle = b"AB\\a.png";
        let mut hits = 0;
        for pos in 0..bytes.len() - needle.len() {
            if &bytes[pos..pos + needle.len()] == needle {
                bytes[pos] = 0x93;
                bytes[pos + 1] = 0xfa;
                hits += 1;
            }
        }
        assert_eq!(hits, 2);
        let err = import_zip(&f.store, "sjis", &bytes).unwrap_err();
        assert!(err.contains("UTF-8"), "{err}");
        assert!(!f.imported("sjis").exists());
        f.staging_empty();

        let bytes = zip_entries(&[("index.html", b"index"), ("a\\b.js", b"x")]);
        let err = import_zip(&f.store, "plain", &bytes).unwrap_err();
        assert!(!err.contains("UTF-8"), "{err}");
    }

    #[test]
    fn failure_leaves_no_residue() {
        let f = Fixture::new();
        let bytes = zip_entries(&[("style.css", b"css")]);
        assert!(import_zip(&f.store, "residue", &bytes).is_err());
        assert!(!f.imported("residue").exists());
        f.staging_empty();

        let bytes = zip_entries(&[("index.html", b"index"), ("a/../../evil.js", b"evil")]);
        assert!(import_zip(&f.store, "residue", &bytes).is_err());
        assert!(!f.imported("residue").exists());
        f.staging_empty();
    }
}
