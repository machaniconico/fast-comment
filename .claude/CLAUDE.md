# fast-comment

マルチプラットフォーム・コメントビューワー（わんコメ相当・より軽量/低遅延）。Tauri 2 + Svelte 5。

## 非自明な制約（エージェントが間違えやすい点）

- **ビルドは Windows 側で行う**。WSL で `tauri build` すると Linux バイナリになる。WSL は編集専用。
- **重い処理は Rust 側に寄せる**。UI に解析/正規化を持ち込まない。UI は描画のみ。
- **YouTube パースは寛容に**。固い struct deserialize 禁止。`serde_json::Value` のパス探索で、欠落しても None で劣化させる。仕様変更は `config.json` の `youtubeOverrides` で再ビルド無しに吸収。
- **X チャット本文は `api.x.com/live-chat` の NDJSON ストリームで取る**（認証不要、`x.rs`）。旧 Periscope chatapi（chatnow WS / history）には本文が一切流れない（2026-08 実測、X が新基盤へ移行済み）。行に username は無くゲスト解決も不可 → 配信者のみ show.json で名前解決、他は「ユーザー下4桁」表示。broadcaster 判定は `twitter_user_id`（`user_id` は Periscope ID で別物）。詳細は memory `x-chatapi-protocol-verified`。
- **Twitch の PING には必ず PONG を返す**（返さないと切断される）。
- **IPC は1フレーム単位でバッチ送出**。1コメント1emitにしない（往復過多）。
- 実モデレーション(BAN/削除)は OAuth 必須で **P6**。MVP はローカル非表示/NGのみ。

詳細仕様は `.claude/SPEC.md`。これが単一情報源。
