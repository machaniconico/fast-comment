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

## 補足(2026-11-25、受け入れテスト作成時の確認)
- `BackupError` と `BackupEntry` は Debug を実装する。
- 起動時に適用が失敗したら Err を返す(ロールバックと `restore-failed-*` への改名は、その前に済ませておく)。
- `.pre-restore-<ms>` は config_dir 直下に置く(対象ごとに `<名前>.pre-restore-<ms>`)。古いものは最新の 3 世代だけ残す。
- 適用する時にも manifest と JSON の妥当性を確かめる。JSON が壊れていればロールバックする。
- 検証に落ちた zip では、事前バックアップも restore-pending も作らない。
- 事前バックアップの中身が最新のバックアップと同じなら、新たに作らない(最新のものが今の状態の写しになっているので)。
- READY の無い restore-pending は適用しない(途中で落ちた展開の残骸とみなし、消す)。
- 新旧の順序と created_at_ms は、ファイル名の数値で決める。

## 修正1 の裁定(2026-11-25、レビュー後。ルート)
- 秘密の検査: 本当の秘密(twitchOauth・twitchClientSecret・youtubeApiKey・xAuthToken・xCsrfToken、xOverrides.bearerToken、youtubeOverrides.apiKey など鍵・トークン・cookie)だけを対象にし、8 文字未満の値は検査しない。ユーザー名・クライアント ID は対象外。
- バックアップに入れる config.json では、credentials に加えて、鍵・トークンを持つ override の値(xOverrides.bearerToken、youtubeOverrides.apiKey 等。実装担当が config.rs を走査して列挙する)も空にする。
- 復元時に「今の値を引き継ぐ」欄: credentials、上の鍵・トークン欄、資格情報と一緒に送られる宛先(xOverrides の URL 類など、cookie やトークンを送る先)、外部プログラムを起動する設定(棒読みちゃんの実行ファイルなど。config から Command / spawn される値をすべて)。zip の値は使わない。SPEC の既知の限界に列挙する。
- prune は復元元の zip を消さない(stage_restore では、復元元を restore-pending に写してから事前バックアップと prune を行う)。
- zip に無い許可対象(listeners.json、templates/ など)は「バックアップの時点で無かった」とみなし、今のものを .pre-restore に退避してから外す。
- 検証は、実際のストアの読み込みと同じ型で行う(Config、listeners の HashMap<String, ListenerRecord>、実績の定義と進捗、sounds.json)。読めなければ Invalid。stage と apply の両方で行う。
- ロールバックが失敗した時(Windows のロックなど): 短い間隔で 3 回試す。それでも失敗した対象があれば、APPLYING の印を残して次回の起動で再び戻しを試みる。今回の起動では、該当するストアを保存禁止(listeners の既存の保存禁止モードと同じ扱い)にして起動し、UI に失敗を出し続ける。
- 起動時の掃除: restore-applied-*.tmp、auto/*.tmp を消す。restore-failed-* と *.pre-restore-* は最新の 3 件だけ残す。
- 上限を超えて自動作成できない時は、UI(バックアップ欄)に最後の失敗の理由を出す。一覧で権限エラーが出た時は空と見せず、エラーを出す。
- 確認ダイアログに次の文言を足す:「事前バックアップの後、再起動までの変更は失われる」「X 自動告知の台帳は戻らない」「実行ファイルの指定や接続先の上書きは今の設定のまま」。

## 修正2 の裁定(2026-11-25、ルート。Codex 再レビュー 2a8ee9d の 7 件)
- 復元に失敗したあとの起動では、自動作成も prune もしない(復元元を残して再試行できるようにする)。
- 名前の照合は大文字小文字を区別しない(Windows)。許可リストと同じ綴りでも、大小違いのエントリ(SOUNDS.JSON など)は Invalid にする。型検証も大小を無視した名前で行う。
- 保存禁止のストアがあるときは、そのストアを変える IPC(リスナー削除・実績のリセットなど)を、何かを変える前に拒否する(片方だけ確定させない)。
- 作成時にも、復元時と同じ型の検証をする。読めないストアがあれば作成は失敗にし、prune もしない(壊れた写しで正常な世代を押し出さない)。UI に理由を出す。
- 復元時に今の値を引き継ぐ欄に `xPost.callbackPort`・`twitcasting.callbackPort`(と、OAuth の redirect に関わる欄すべて)を足す。
- URL の欄にユーザー情報(user:pass@)が入っている場合は、zip ではユーザー情報を消す。復元時は、その欄の今の値を引き継ぐ(voicevoxUrl など、config.rs の URL 欄を走査して列挙する)。
- 既知の限界(SPEC に書く): 配信中のバックアップでは、リスナー台帳と実績の進捗の写しの間にコメントが入ると、件数が 1 件ほどずれることがある。
- MINOR: 起動時の掃除は、1 件の削除失敗で残りを打ち切らない。自動作成の失敗理由は、成功したら消す。手動作成の失敗理由も UI に出す。

## 修正2 の実施記録(2026-11-25、Opus)
- 1: `backup::run_backup` に集約。起動時の復元失敗(`AppState.restore_failed_at_startup`、通知を閉じても変わらない)の後は Startup・Interval とも Ok(false)。手動作成は行うが prune しない。
- 2: `validate_path` で、ルートは FILES・DIRECTORIES・manifest.json、`sounds/` 直下は sounds.json と、`to_uppercase` で一致して綴りが違う名前を Invalid(`ſ` も含む)。`validate_stores` は大小を無視した名前で型を引く。
- 3: 保存禁止の確認を変更より前に置いた IPC: update_listener・set_listener_voice・reset_listener_donation・reset_all_listener_donations・delete_listener(台帳と進捗の両方)・commit_achievements(定義と進捗、SaveRefused を返す)。テンプレート・効果音は既に先頭で確認済み(静的テストで固定)。設定は commit_config が保存成功後にだけメモリを差し替えるので対象外。
- 4: 作成時も `validate_stores`。失敗理由はストアごと(設定・リスナー記録・実績の定義・実績の進捗・効果音の一覧の形式)。
- 5: 引き継ぐ欄に xPost.callbackPort・twitcasting.callbackPort。YouTube は空きポートの loopback、Twitch はデバイスコードで、config に redirect の欄は無い。Client ID は credentials 内で引き継ぎ済み。
- 6: URL の欄 = tts.options.voicevoxUrl、xOverrides.endpoints、niconicoOverrides.endpoints、twitcastingOverrides.endpoints。zip では `://` の後の authority の最後の `@` までを消す。復元時はユーザー情報の有無にかかわらず今の値(zip の値は消した後なので使わない)。チャンネルの identifier と bouyomiHost は URL の欄ではないので対象外。
- MINOR: 掃除は最初の失敗を記録して続ける。自動作成の状態は `auto-backup-status`(成功で null)。手動作成の失敗理由を表示。

## 修正3 の裁定(2026-11-25、ルート。Codex 再レビュー(392d9f4)の FAIL 5 件)
1. ブロッカー: 復元の失敗後に別の世代を復元すると、最初の復元元が消える。復元の準備(`stage_restore`)と、復元を適用するまでの間は prune を一切しない。また、`restore-failed-*` で退避した世代と、復元に失敗した元の世代は、prune の対象から外す(次に復元が成功した起動で通常の扱いに戻す)。テスト: keep=1 で A の復元が失敗 → B を復元に準備 → A が残っていること。
2. MINOR(直す): X の「切断」は、config が保存禁止の時は、資格情報を消す前に拒否する(ほかの書き込みの IPC と同じ事前の確認)。
3. MINOR(直す): 公式投票の開始(Twitch・YouTube)は、新しい質問を config に保存する必要がある時、config が保存禁止なら遠隔で作成する前に拒否する。
4. MINOR(直す): URL のユーザー情報の除去は `url::Url::parse`(または reqwest と同じ解釈)で行う。解析できた時に username か password があれば、それを除く。解析できない値で `@` を含むものは、安全側に倒して値ごと空にして zip に書く(復元の時は今の値を引き継ぐ)。テスト: `https:///u:p@h`、`https://u:pa/ss@h`、`user:pass@host:50021`。
5. MINOR(既知の限界): 効果音の削除と自動作成が重なった時の、索引と本体のずれ。sounds に既存のロックがあれば、バックアップで `sounds/` を読む間はそれを持つ。無ければ SPEC の既知の限界に書く。

## 修正3 の実施記録(2026-11-25、Opus)
- 1: `stage_restore` から prune を外し、`restore-pending` に復元元の名前 `SOURCE` を書く。`prune` は `restore-pending` がある間は何も消さず、`restore-failed-*/SOURCE` に記録された zip を残す。起動時の掃除は SOURCE のある失敗世代を 3 世代の整理から外し、復元が成功した起動で SOURCE を消して通常の扱いに戻す。
- 2: `config::ensure_save_allowed`(`save` と同じ判定)を足し、`disconnect_x_post` は API モードの時に資格情報を消す前に確認する。
- 3: `PreferenceChange::Check` を足し、`PollControl::start` が遠隔作成の前に呼ぶ。質問が変わる時だけ保存禁止を見る。
- 4: `url::Url::parse` で解析し、ユーザー情報を消す。解析できない値(ホストを持たない値を含む)で `@` を含むものは空にする。
- 5: `SoundStore` に既存のロックがあるので、作成(`run_backup` の flush の後)と復元の準備の間、`lock_for_backup` で持つ。SPEC の既知の限界には足していない。
