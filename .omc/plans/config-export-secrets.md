# 設定エクスポートの資格情報(PR #68)

仕様の正本は `.claude/SPEC.md` §8.1。ここにはレビューの裁定と、その実装だけを残す。

## 初回(01b94a1)

- 漏れていた欄: `credentials` 全体、`xOverrides.bearerToken`、`youtubeOverrides.apiKey`、URL の 4 欄のユーザー情報。
- 書き出しは `export_config`、読み込みは `import_config`。伏せる欄の一覧と URL の処理は `backup.rs` の自動バックアップと共有する。
- 旧版の書き出しは読み込めるが、中の資格情報は適用しない。

## レビュー 1 回目(Codex Sol xhigh: FAIL 1 件 / Opus critic: APPROVE + MINOR)

### ブロッカー: 読み込みで X の自動告知が有効になる

- 裁定: 読み込みは X の自動告知を必ず無効にする。ファイルが有効でも、今が有効でも無効。固定文言を UI に出す。
- 実装: `backup::merge_import` が最後に `x_post.enabled = false` にする。`save_config_update` は、ファイルか今の設定のどちらかで有効だった時に `X_POST_DISABLED_BY_IMPORT` を `warnings` に足し、UI は既存の `warnings` 表示でそのまま出す。両方とも無効だった時は何も変わらないので出さない。
- テスト: `settings_import_never_enables_x_autopost`(`xPost.enabled` と API モード、他の配信者の `ownStream` を持つファイル。今が有効・無効の両方)。

### MINOR

1. 新しい書き出しを v0.8.0 以前が読み込むと資格情報が消える
   - 裁定: 旧版が拒否するよう形式を上げる。新版は新旧どちらも読む。
   - 実装: 旧版は `schema` の不一致だけを見て `version` を見ないので、`schema` を `fast-comment/settings-v2` に変えた(`version` は 2)。読み込みは `fast-comment/settings` と schema なしも受け付ける。
2. `xPost.callbackPort` と `twitcasting.callbackPort` は今の値のまま
   - 実装: `LOCAL_CONFIG_FIELDS` から `CALLBACK_PORTS` を分け、復元と読み込みの両方が通る `kept_fields()` に入れた。
3. 今の値のままにする URL は `xOverrides.endpoints` だけ
   - 実装: `X_ENDPOINTS` を `kept_fields()` に入れた。`voicevoxUrl`、`niconicoOverrides.endpoints`、`twitcastingOverrides.endpoints` はファイルの値を使い、`redact_overrides`(書き出しと同じ関数)でユーザー情報を消す。復元(zip)は従来どおり URL の 4 欄とも今の値。
4. `export_config` / `import_config` はメインウィンドウだけ
   - 実装: `window.label() != "main"` なら固定文言で拒否。
5. YouTube 接続との競合
   - 原因: `connect_youtube_oauth` は `config_write` を取らず、設定ロックだけで保存する。読み込みが検証の前に読んだ `credentials` で、その保存を戻し得た。
   - 実装: 差し替えを型変換後の `AppConfig` に対して行う `merge_import(&incoming, &current)` にし、`commit_config` と同じ設定ロックの中で呼ぶ。検証(API モードの接続確認)が保存する Client ID を見るよう、検証の前にも一度呼ぶ(2 回通しても結果は変わらない。テストあり)。
   - 型変換後に差し替えるので、配列で書かれた欄の拒否(`keep_current_redacted` の親の検査)は不要になり、消した。
6. 残存検査の失敗と空白
   - 実装: `RegexSet` の組み立て失敗は `EXPORT_CHECK_ERROR`(別の固定文言)。`collect_secret` は前後の空白を除いてから 8 文字の判定と照合をする。zip の検査も同じ関数なので同じく変わる。
7. 今の設定に `bearerToken`・`endpoints` が無く、ファイルにある場合
   - テスト: `settings_import_drops_file_secrets_when_current_has_none`(鍵と X の接続先は取り込まない。X 以外の接続先はファイルの値)。

## 追加の裁定(57fbfde の後)

### 棒読みちゃんの起動パス

- 経路: `tts.options.bouyomiPath` と `bouyomiLaunchElevated` にファイルの値が入り、バックエンドが棒読みちゃんなら、未起動の時に保存の直後にそのパスを起動していた(昇格つきもあり得た)。
- 裁定: 復元と同じく、読み込みでも今の値を残す。一覧は復元と共有する。
- 実装: `LOCAL_CONFIG_FIELDS` を `kept_fields()` に入れた。復元(`prepare_config`)と読み込み(`merge_import`)が同じ `kept_fields()` を通るので、一覧は 1 か所。
- テスト: `settings_import_keeps_current_bouyomi_launch`(ファイルに別のパスと `bouyomiLaunchElevated: true`。今のパスが空・設定済みの両方)。

### X 以外の URL のユーザー情報

- 裁定: 今のまま。接続先が今と同じでも今のユーザー情報は引き継がず、ファイルの値からユーザー情報を消して入れる。
