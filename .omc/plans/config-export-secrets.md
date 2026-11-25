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

## 修正3 の裁定(2026-11-25、ルート。Codex 再レビュー(bdfa04c)の FAIL 5 件、critic は APPROVE)
重大度の判定: 下の 1 と 4 はタイミングや異常な環境が重なった時だけ起きるので MINOR とする。2・3・5 は通常の取り込みで起きるので直す。
1. MINOR(直さない): 無効化の保存と配信開始が同時に来ると、旧設定(取り込む前の自分の設定)で投稿する可能性がある。投稿するのは自分の設定のままなので、既知の限界として SPEC に 1 行書く。
2. 直す: 取り込みでは、保存後の棒読みちゃんの自動起動をしない(通常の設定保存では今のまま)。取り込みで TTS のバックエンドが棒読みちゃんになった時は、「棒読みちゃんは自動では起動しません」と案内を warnings で返す。
3. 直す: 読み上げの本文の送り先。`tts.options.voicevoxUrl` と `bouyomiHost` は、ファイルの値のホストがループバック(localhost、127.0.0.0/8、::1)の時だけファイルの値を使う。それ以外のホストなら今の値を残し、warnings で知らせる(「読み上げの接続先は、この PC 以外を指していたため今の設定のままにしました」)。ポート(bouyomiPort と URL 内のポート)はファイルの値でよい(AivisSpeech の 10101 を移すため)。
4. MINOR(直さない): OBS のポート競合で bind に失敗した時の食い違い。これは既存の update_config と同じ挙動なので、この PR の範囲外とする。
5. 直す(critic の MINOR 1 と同じ): X に未接続の PC で API モードのファイルを取り込んだ時は、xPost.mode を今の値のまま残す(enabled は false なので投稿は起きない)。取り込み全体は拒否しない。warnings で「X の投稿方式は今の設定のままにしました」と出す。
6. 直す(Codex の MINOR): `config` がオブジェクトでない時(`{"config":[]}` など)は、Rust 側でも固定の文言で拒否する。
7. 直す(critic の MINOR): onImported が false を返した時も、warnings の通知を出す。書き出しから `tts.options.bouyomiPath` を除く(取り込み時には今の値を残すので不要。Windows のユーザー名を含みうる)。X の自動告知を無効にした通知の文に「有効にする前に、自分の配信(ownStream)の設定も確認してください」を足す。

### 修正3 の実装

1. 直さない。SPEC §8.1 に既知の限界として 1 行足した。
2. `launches_bouyomi_after_save(saved, imported)` が、取り込みでは false を返す(通常の保存は従来どおり)。`backup::import_notices` が、取り込みでバックエンドが棒読みちゃんに変わった時に案内を返す(今も棒読みちゃんなら出さない)。
   - テスト: `import_does_not_launch_bouyomi`(lib.rs)、`settings_import_keeps_current_bouyomi_launch` の通知の確認。
3. `merge_import` が型変換後の設定に対して、`voicevoxUrl` は `url::Url` で解析したホスト、`bouyomiHost` は文字列が、`localhost`・127.0.0.0/8・`::1` の時だけファイルの値を使う。解析できない値は今の値を残す。通知は、今の値と違っていた時だけ出す(同じ PC で書き出して読み込んだ時に出さない)。`bouyomiPort` はそのままファイルの値。
   - テスト: `settings_import_takes_tts_destination_only_from_this_pc`。
4. 直さない(範囲外)。
5. `settle_x_post_mode` が、取り込みの時だけ `validate_x_post_mode` の拒否を「`xPost.mode` を今の値に戻す + 通知」に変える。通常の保存は従来どおり拒否する。
   - テスト: `import_keeps_x_post_mode_instead_of_rejecting`(lib.rs)。接続状態を持つ `AppState` はテストで組めないので、検証結果を引数で受ける関数に分けて確かめている。
6. `backup::parse_import` が、オブジェクトでない `config` を型変換の前に「設定の形式が不正です (項目 .)」で拒否する。UI の既存の型エラー表示にそのまま出る。
   - テスト: `settings_import_rejects_a_config_that_is_not_an_object`。
7. 3 点。
   - 読み直しに失敗した時の通知: 文の組み立てを `src/lib/configPortability.ts` の `importMessage` に出し、`onImported` が false でも `warnings` があれば「設定は保存しました。」に続けて出す。読み直しを求める帯と並べて表示する(これまでは帯の間、状態の文を隠していた)。テスト: `src/lib/configPortability.test.ts`(`pnpm test` の一覧に追加)。
   - 書き出しから `tts.options.bouyomiPath` を除いた(`BOUYOMI_PATH`)。`bouyomiLaunchElevated` は裁定に無いので書き出しに残している(読み込みは今の値を残す)。テスト: `settings_export_contains_no_secret_value`。
   - X の自動告知を無効にした通知に ownStream の文を足した。テスト: `settings_import_never_enables_x_autopost`。
- 通知の 4 文は、UI が 1 行につなげて出すので、末尾に「。」を付けた。
