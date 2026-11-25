# 自動バックアップ(2026-11-25、ルート決定。R3: ユーザーデータを書き戻す・削除する、永続ファイルを扱う)

前提:
- 規約は `.claude/CLAUDE.md`、仕様は `.claude/SPEC.md` の §10(設定永続化)。
- 受け入れテストは `src-tauri/tests/acceptance/backup*.rs` に置く。実装担当はこのテストを編集しない。

## 決めたこと

### 保存先・頻度・世代数
- 保存先は `<app_config_dir>/backups/auto/`。ファイル名は `fc-backup-<UTC ミリ秒>.zip`。
- 頻度:
  - 起動時に、最新のバックアップが 20 時間以上前なら(または 1 件も無ければ)1 回作る。
  - 起動中は 24 時間ごとに作る。
  - 前回と中身(全エントリの SHA-256)が同じなら作らない。
- 世代: 直近 `keep` 件を残し、古いものから消す。
  - 消すのは `backups/auto/` の中の `fc-backup-<数字>.zip` だけ。名前が合わないファイルには触れない。
- 設定: `config.json` の `backup: { enabled: bool(既定 true), keep: u32(既定 10、範囲 1〜50。範囲外は丸める) }`。

### 中身
- zip には `manifest.json` を入れる: `{ schema: "fast-comment-backup", version: 1, createdAtMs, appVersion, entries: [相対パス] }`。
- 入れるもの(あるものだけ):
  - `config.json`。ただし `credentials` を既定値(空)に置き換えた版。
  - `listeners.json`、`achievements.json`、`achievement-progress.json`、`sounds.json`
  - `sounds/` 配下のファイル
  - `templates/` 配下(カスタムテンプレート。ユーザーデータ側)
  - `skin/` 配下
- 入れないもの:
  - `integrations.json`(ローカル API と Webhook。URL と署名の秘密を含むため)
  - `x-autopost-ledger.json`(古い台帳に戻すと、二重投稿の防止が崩れるため)
  - `logs/`、`exports/`、`backups/`
  - `*.tmp`、`*.broken-*`、`*.corrupt-*`、`*.bak.*`
  - 資格情報ストアの中身
- 秘密の確認: 書き出した zip の全エントリの中に、`config.json` の `credentials` の各値(空でないもの)が 1 つも含まれないこと。テストで確かめる。
- 作成は一時ファイルに書いてから rename で公開する。途中で失敗したら一時ファイルを消す。
- 失敗してもアプリは止めない。warn ログを出す(パスは相対名だけ)。

### 復元
- 復元は UI の IPC からだけ行う。ローカル API・Webhook・テンプレ zip の取り込みからは呼べない。
- 手順:
  1. zip を検証する。
     - manifest の schema と version
     - エントリのパスは相対で、`..` も絶対パスも含まず、ドライブ文字もバックスラッシュもないこと
     - 許可リストの外のパスは拒否する
     - エントリ数 2000 以下、展開後の合計 200MB 以下、1 エントリ 50MB 以下
     - symlink は拒否する
  2. 復元の前に、今の状態を `fc-backup-<ms>.zip` として必ず 1 件作る(世代の数え方は同じ)。
  3. `<app_config_dir>/backups/restore-pending/` に展開し、マーカー `restore-pending/READY` を書く。
  4. アプリを再起動する。
  5. 起動時、ストアを読み込む前に `READY` があれば適用する。
     - 対象のファイルとディレクトリを `.pre-restore-<ms>` に退避してから、rename で置き換える。
     - `config.json` の `credentials` は、今のファイルの値を引き継ぐ(バックアップ側は空なので、上書きしない)。
     - 適用が済んだら `restore-pending/` を消す。
     - 失敗したら退避から戻し、`restore-pending/` を `restore-failed-<ms>/` に改名してエラーを記録する。UI には「復元に失敗しました」を 1 度だけ出す。
- 再起動を挟む理由: 稼働中のストア(listeners の 5 秒 flush など)が、復元したファイルを上書きするのを防ぐため。

### UI
- 設定画面の、既存の設定の書き出し・取り込み(`ConfigPortability`)の近くに置く。
  - 有効のオン・オフ、世代数
  - 一覧(日時、サイズ)
  - 「今すぐ作成」、「復元…」(確認ダイアログ: 再起動すること、今の状態を先にバックアップすること、資格情報とローカル API・Webhook の設定は戻らないこと)
  - 「フォルダを開く」

## 公開の境界(受け入れテストはここだけを使う)
- `backup` モジュール(新規。`lib.rs` から `pub mod backup`)
  - `pub struct BackupSettings { pub enabled: bool, pub keep: u32 }`
    - `Default` は true / 10。
    - `pub fn normalized(self) -> Self` で keep を 1〜50 に丸める。
  - `pub fn create_backup(config_dir: &Path, now_ms: u64) -> Result<Option<PathBuf>, BackupError>`
    - 中身が前回と同じなら `Ok(None)` を返す。
    - 世代の整理は別関数で行う。
  - `pub fn prune(config_dir: &Path, keep: u32) -> Result<usize, BackupError>`: 消した件数を返す。
  - `pub fn list_backups(config_dir: &Path) -> Vec<BackupEntry>`
    - `BackupEntry { pub file_name: String, pub created_at_ms: u64, pub size: u64 }`
    - 新しい順に並べる。
  - `pub fn due_at_startup(config_dir: &Path, now_ms: u64) -> bool`: 20 時間の規則を判定する。
  - `pub fn stage_restore(config_dir: &Path, backup_file_name: &str, now_ms: u64) -> Result<(), BackupError>`
    - 検証、事前バックアップ、restore-pending への展開、READY の書き込みまで行う。
    - `backup_file_name` は `fc-backup-<数字>.zip` の形だけを受け付ける。パスは受け付けない。
  - `pub fn apply_pending_restore(config_dir: &Path, now_ms: u64) -> Result<bool, BackupError>`
    - 起動時に呼ぶ。適用したら true、何も無ければ false を返す。
  - `pub enum BackupError { Io(String), Invalid(&'static str), NotFound }`
    - `Display` は日本語で、絶対パスと秘密を含めない。

## 受け入れ基準(テスト名の粒度、backup.rs)
- `backup_contains_expected_entries_and_manifest`: config・listeners・achievements・sounds/・templates/・skin/ が入っていること。integrations.json・ledger・logs/・exports/・*.tmp が入っていないこと。
- `backup_strips_credentials`: config.json の credentials に xAuthToken・xCsrfToken・twitchOauth・youtubeApiKey を入れておく。zip の全エントリのどこにもそれらの値が無く、zip 内の config.json の credentials が既定値であること。
- `unchanged_content_is_not_backed_up_twice`
- `prune_keeps_newest_and_ignores_foreign_files`: keep=2 で 4 件あれば 2 件を消す。`notes.txt` と `fc-backup-abc.zip` は残す。
- `settings_keep_is_clamped`(0 は 1、99 は 50)
- `due_at_startup_after_20_hours_or_when_empty`
- `stage_restore_rejects_bad_names_and_traversal`:
  - `../x.zip`、`C:\\a.zip`、`fc-backup-1.zip/../x` は Invalid。
  - 存在しない名前は NotFound。
  - zip 内に `../evil`、`/abs`、許可外の `integrations.json` があれば Invalid。
- `stage_restore_takes_safety_backup_first`
- `apply_restore_replaces_files_and_keeps_current_credentials`: 復元後、listeners と achievements はバックアップの内容に、config の credentials は今の値になること。`restore-pending/` が消えていること。`.pre-restore-*` があること。
- `apply_without_pending_is_false`
- `failed_apply_rolls_back`: READY があっても中身が壊れていたら、元のファイルに戻り、`restore-failed-*` が残ること。
- `errors_do_not_contain_absolute_paths`

## 実装側の受け入れ基準(レビューで確かめる)
- 起動時の `apply_pending_restore` は、config・listeners・achievements・sounds・templates の読み込みより前に呼ぶ。
- 定期作成は別タスクにして、`app_cancel` で止める。zip の作成は `spawn_blocking` で行う。
- 復元の IPC の後は `app.restart()` を使う。
- ローカル API と Webhook から呼べないこと。
- ログに絶対パスと秘密を出さない。
- SPEC §10 にバックアップの節を足す(対象、除外とその理由、復元の手順、既知の限界)。既知の限界は次の 2 点。
  - 資格情報・ローカル API・Webhook は戻らない。
  - 台帳は戻さない。
- UI の文言は日本語にする。
- pnpm test と pnpm check を通す。
