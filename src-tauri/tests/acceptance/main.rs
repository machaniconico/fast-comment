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

#[test]
fn acceptance_harness_runs() {}
