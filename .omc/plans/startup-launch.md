# 起動時に他のアプリと URL を開く(2026-11-25、ルート決定。R3: 外部プログラムを起動する)

ユーザー決定(2026-11-25):
- 確認は、登録するときと変更するときに 1 回だけ出す。それ以降は、アプリを起動するたびに自動で開く。
- 対象は、実行ファイル(引数を付けられる)と URL。
- UI は、設定画面に URL やファイルパスを貼り付ける欄を作る。

前提: 規約は `.claude/CLAUDE.md`、仕様は `.claude/SPEC.md`(§18 を新しく作る)。受け入れテストは `src-tauri/tests/acceptance/startup_launch*.rs` に置き、実装担当は編集しない。

## 決めたこと

### 保存
- 保存先は `<config_dir>/launch.json`。`integrations.json` と同じく専用のファイルにする。
  - `AppConfig`(config.json)と `update_config` には載せない。
  - 自動バックアップと復元の対象にも入れない(`backup.rs` の FILES に足さない。SPEC の既知の限界に「起動時に開く設定は戻らない」と書く)。
- 形式: `{ "version": 1, "items": [ { "id": "<uuid>", "kind": "exe" | "url", "target": "<絶対パス または URL>", "args": ["..."], "enabled": true, "label": "<表示名、任意>" } ] }`
- 項目は 20 件まで。`target` は 2048 文字まで。`args` は 32 個まで、1 個 1024 文字まで。`label` は 100 文字まで。
- 書き込みは、一時ファイルに書いてから rename で公開する。

### 承認(改ざん検知)
- 保存するときに、`items` を正規化した JSON(キー順を固定し、`label` は除く)を作り、その HMAC-SHA256 を計算する。
- 鍵は 32 バイトの乱数。鍵と HMAC は資格情報ストア(`secret_store`、user は `launch-approval`)に `{ key, mac }` として置く。
- 起動時に `launch.json` を読み、HMAC が一致したときだけ実行する。
  - 一致しない、ストアの値が無い、読めない、`launch.json` が壊れている、のどれかに当たったら何も起動しない。そのうえで UI に「起動時に開く設定が変更されています。設定画面で確認してください」を出し続ける。
  - 再承認は設定画面から行う(確認ダイアログで全項目をそのまま見せ、承認すると HMAC を計算し直す)。
- HMAC が守るのは、`launch.json` を手で書き換えた場合と、ほかの経路から書き換えた場合だけ。同じ OS ユーザーの権限で動くマルウェアは対象外。これを SPEC に書く。

### 書き込みの経路
- 登録・変更・削除・再承認の IPC は `window: tauri::WebviewWindow` を受け取り、`label == "main"` でなければ拒否する(`require_main_integrations_window` と同じ形)。
- 次の経路からは書き込めないようにする。
  - ローカル API と Webhook にはルートを作らない。
  - テンプレの zip 取り込み、自動バックアップの復元、設定のインポート(ConfigPortability)の対象に入れない。
- IPC は「承認つき保存」の 1 本にまとめる。UI は確認ダイアログで承認した後にだけ、これを呼ぶ。Rust 側では、引数 `confirmed: true` が無ければ拒否する(UI を飛ばして呼ばれる事故を防ぐため)。
- 項目ごとの有効・無効の切り替えも内容の変更に含める。確認ダイアログを出してから保存する(一覧を見せる)。

### 実行
- 実行するタイミング: `setup` の中で、設定を読み込んだ後に 1 回だけ。棒読みちゃんの自動起動(lib.rs:3835 付近)の近くで、非同期に行う。
  - single-instance で 2 回目に起動されたときは実行しない。
  - アプリの再起動(復元など)で起動したときは、通常の起動と同じ扱いにする。
- exe の場合:
  - `target` は絶対パスで、実在する通常ファイルであること。拡張子は `.exe` だけを受け付ける(大文字小文字は区別しない)。`.bat`、`.cmd`、`.lnk`、`.ps1`、`.vbs`、`.js`、`.msi` などは、登録の時点で拒否する。
  - `Command::new(target).args(args)` で起動する。シェルや `cmd /c` は使わない。作業ディレクトリは exe の親フォルダ。
  - 管理者権限での起動はサポートしない。
  - 起動に失敗しても、アプリは止めない。warn ログ(項目の label と失敗の種類だけを出し、パスや引数は出さない)と、UI への通知を出す。
- URL の場合:
  - `http://` と `https://` だけを受け付ける。ユーザー情報(user:pass@)が付いていたら拒否する。
  - 開くのは既存の `update::open_url`(ShellExecuteW)。
- 項目は登録した順に、間を 300ms あけて実行する。無効な項目は飛ばす。

### UI(設定画面)
- 置き場所は「起動時に開く」の欄。ツールか全般のタブ。
- 追加の欄: 種類(アプリ / URL)、パスまたは URL の貼り付け欄、引数(1 行に 1 つ)、表示名。
- 一覧: 各行に有効・無効、削除、上下の並べ替えを付ける。
- 保存するときは、確認ダイアログに全項目の種類・パス・引数・URL を省略せずに出す。文言の例:「次のアプリと URL を、fast-comment を起動するたびに自動で開きます。信頼できるものだけを登録してください。」
- 再承認が必要なときは、目立つ警告と「内容を確認して承認」ボタンを出す。
- パスは貼り付けで入れる(ファイル選択のダイアログは今回は入れない)。

## 公開の境界(受け入れテストはここだけを使う)
- `startup_launch` モジュール(新規。`lib.rs` から `pub mod startup_launch`)
  - `pub enum LaunchKind { Exe, Url }`(serde では "exe" / "url")
  - `pub struct LaunchItem { pub id: String, pub kind: LaunchKind, pub target: String, pub args: Vec<String>, pub enabled: bool, pub label: Option<String> }`
  - `pub fn validate_items(items: &[LaunchItem]) -> Result<(), LaunchError>`: 上限・種類・拡張子・URL の規則を確かめる。exe の実在は確かめない(実在の確認は実行時に行う)。
  - `pub fn approval_mac(key: &[u8], items: &[LaunchItem]) -> Vec<u8>`: 正規化した JSON の HMAC。label が違っても同じ値、ほかの欄が違えば違う値になる。
  - `pub trait ApprovalStore { fn load(&self) -> Result<Option<(Vec<u8>, Vec<u8>)>, LaunchError>; fn save(&self, key: &[u8], mac: &[u8]) -> Result<(), LaunchError>; }`(テストではメモリで実装したものを渡す)
  - `pub fn save_approved(config_dir: &Path, store: &dyn ApprovalStore, items: Vec<LaunchItem>, confirmed: bool) -> Result<(), LaunchError>`: `confirmed` が false なら拒否する。validate、新しい鍵の生成、mac の計算、`launch.json` の書き込み、ストアへの保存を行う。
  - `pub fn load_for_startup(config_dir: &Path, store: &dyn ApprovalStore) -> StartupDecision`
    - `pub enum StartupDecision { Nothing, Run(Vec<LaunchItem> /* enabled のものだけ、登録順 */), NeedsApproval }`
    - ファイルが無ければ Nothing。HMAC が一致すれば Run。それ以外(不一致・ストアの値が無い・壊れた JSON・version が違う・validate に落ちる)は NeedsApproval。
  - `pub fn load_items(config_dir: &Path) -> Result<Vec<LaunchItem>, LaunchError>`(UI 表示用。承認は問わない)
  - `pub enum LaunchError { NotConfirmed, Invalid(&'static str), Io(String), Store(String) }`。Display は日本語で、パス・引数・URL を含めない。

## 受け入れ基準(テスト名の粒度、startup_launch.rs)
- `validate_accepts_exe_and_https_url`
- `validate_rejects_non_exe_extensions`: `.bat`、`.cmd`、`.lnk`、`.ps1`、`.vbs`、`.msi`、拡張子なし、`.EXE` は受け入れる(大文字小文字を区別しない)。
- `validate_rejects_relative_paths`: `app.exe`、`.\\app.exe`、`..\\x.exe`。`C:\\Tools\\app.exe` と `\\\\server\\share\\app.exe` は受け入れる。WSL でテストできるように、絶対パスの判定は Windows の規則で行う関数にする。
- `validate_rejects_bad_urls`: `file:///`、`javascript:`、`ftp://`、`https://user:pass@x`、空。
- `validate_limits`: 項目 20 / 21 件、target 2048 / 2049 文字、args 32 / 33 個、1 個 1024 / 1025 文字、label 100 / 101 文字(chars で数える)。
- `mac_ignores_label_but_covers_everything_else`: id・kind・target・args(順番を含む)・enabled のどれかが変われば mac が変わる。label だけなら変わらない。
- `save_requires_confirmation`: confirmed=false なら NotConfirmed で、ファイルもストアも変えない。
- `startup_runs_only_when_approved`: 保存した直後は Run(enabled の項目だけ、登録順)。
- `tampered_file_needs_approval`: `launch.json` の target・args・enabled を書き換えたら NeedsApproval。label だけなら Run のまま。
- `missing_store_or_corrupt_file_needs_approval`: ストアが空、壊れた JSON、version 2、validate に落ちる内容は、どれも NeedsApproval。
- `no_file_is_nothing`
- `errors_do_not_leak_targets_or_args`

## 実装側の受け入れ基準(レビューで確かめる)
- IPC は main ウィンドウだけに許し、`confirmed: true` を必須にする。ローカル API・Webhook・テンプレ zip・バックアップ復元・設定のインポートには、書き込みの経路が無いこと。
- `launch.json` は自動バックアップの対象に入らないこと(PR #63 が先にマージされていれば、backup.rs の FILES に入っていないことを確かめる)。
- 実行は setup で 1 回だけ行う。single-instance の 2 回目では実行しない。シェルを経由しない。`.exe` が実在する通常ファイルであることを実行の直前に確かめる。
- ログ・エラー・emit にパス・引数・URL を出さない(表示名と失敗の種類だけ)。
- UI の確認ダイアログでは、全項目を省略せずに見せる。再承認の警告を出す。
- SPEC §18 を新しく作る(保存先・承認・書き込み経路の制限・実行の規則・既知の限界)。
- `corepack pnpm test` と `corepack pnpm check` が通ること。
