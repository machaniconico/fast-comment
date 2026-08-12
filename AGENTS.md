# fast-comment

マルチプラットフォーム・コメントビューワー（わんコメ相当・より軽量/低遅延）。Tauri 2 + Svelte 5。

## 非自明な制約（エージェントが間違えやすい点）

- **ビルドは Windows 側で行う**。WSL で `tauri build` すると Linux バイナリになる。WSL は編集専用。
- **重い処理は Rust 側に寄せる**。UI に解析/正規化を持ち込まない。UI は描画のみ。
- **YouTube パースは寛容に**。固い struct deserialize 禁止。`serde_json::Value` のパス探索で、欠落しても None で劣化させる。仕様変更は `config.json` の `youtubeOverrides` で再ビルド無しに吸収。
- **Twitch の PING には必ず PONG を返す**（返さないと切断される）。
- **IPC は1フレーム単位でバッチ送出**。1コメント1emitにしない（往復過多）。
- 実モデレーション(BAN/削除)は OAuth 必須で **P6**。MVP はローカル非表示/NGのみ。

詳細仕様は `.claude/SPEC.md`。これが単一情報源。
