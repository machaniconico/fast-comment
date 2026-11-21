//! 受け入れテストの束ね役。各機能の受け入れテストは `mod <name>;` でここに足す。
//! 受け入れテストは基準作成側(test-engineer)が書き、実装者は編集しない。

mod common;

// PR1: ローカル API の土台と認証(設計書 §5.3)
mod api_gate;
mod integrations_store;
mod obs_boundary;

// PR2: コメント取得と SSE(設計書 §5.4)
mod api_events;
mod api_feed;

// PR3: アプリ内操作(設計書 §5.5)
mod api_ops;
// PR4: Webhook の中核と保存(設計書 §5.6)
mod webhook_common;
mod webhook_delivery;
mod webhook_payload;
mod webhook_rules;
mod webhook_store;
mod webhook_url;

// PR1(ツイキャス): OAuth と保存(設計書 twitcasting §5.2)
mod twitcasting_auth;
mod twitcasting_config_mode;

// PR3'(ツイキャス): 公式 API ポーリングと mode 切替(.omc/plans/pr3-acceptance.md)
mod twitcasting_common;
mod twitcasting_api;
mod twitcasting_budget;
mod twitcasting_common_official;
mod twitcasting_cursor;
mod twitcasting_official_map;
mod twitcasting_official_source;
mod twitcasting_mode;
mod twitcasting_identifier_secrecy;

// 枠ごとの設定上書き (OV-001)
mod channel_overrides_common;
mod channel_overrides_config;
mod channel_overrides_commit;
mod channel_overrides_relay;
mod channel_overrides_tts;

#[test]
fn acceptance_harness_runs() {}
