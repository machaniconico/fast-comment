//! モデレーション層(MVP=ローカル処理のみ)。
//!
//! - NGワード/NGユーザー(正規表現)にマッチしたら隠す。
//! - ハイライトルール(正規表現)にマッチしたら flag を立てる。
//! - 手動のローカル非表示は ID 集合で管理する。
//!
//! ⚠️ 実 BAN/タイムアウト(Twitch)・コメ削除(YouTube)は OAuth 必須=フェーズ2。
//!    ここでは送信前のローカル判定のみを行う。

use std::collections::HashSet;

use regex::Regex;

use crate::config::ModerationConfig;
use crate::model::ChatMessage;

/// 1メッセージに対する判定結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// そのまま流す。
    Show,
    /// 流さない(NG/手動非表示)。
    Hide,
    /// 流すがハイライト扱い(UI/OBS 側で強調)。
    Highlight,
}

/// コンパイル済みルールを保持するモデレータ。
pub struct Moderator {
    ng_words: Vec<Regex>,
    ng_users: Vec<Regex>,
    highlights: Vec<Regex>,
    /// 手動でローカル非表示にしたメッセージ ID。
    hidden_ids: HashSet<String>,
}

impl Moderator {
    /// 設定から正規表現をコンパイルして生成する。
    ///
    /// 不正な正規表現は警告ログを出してスキップ(全体は止めない)。
    pub fn new(config: &ModerationConfig) -> Self {
        Moderator {
            ng_words: compile_all(&config.ng_words, "ngWords"),
            ng_users: compile_all(&config.ng_users, "ngUsers"),
            highlights: compile_all(&config.highlights, "highlights"),
            hidden_ids: HashSet::new(),
        }
    }

    /// 設定を差し替える(手動非表示の集合は維持)。
    pub fn update_config(&mut self, config: &ModerationConfig) {
        self.ng_words = compile_all(&config.ng_words, "ngWords");
        self.ng_users = compile_all(&config.ng_users, "ngUsers");
        self.highlights = compile_all(&config.highlights, "highlights");
    }

    /// 個別コメントを手動でローカル非表示にする。
    pub fn hide_id(&mut self, id: impl Into<String>) {
        self.hidden_ids.insert(id.into());
    }

    /// 手動非表示を解除する。
    pub fn unhide_id(&mut self, id: &str) {
        self.hidden_ids.remove(id);
    }

    /// メッセージを判定する。
    pub fn judge(&self, msg: &ChatMessage) -> Verdict {
        // 手動非表示が最優先。
        if self.hidden_ids.contains(&msg.id) {
            return Verdict::Hide;
        }

        // NGユーザー(名前 or ID にマッチ)。
        if self
            .ng_users
            .iter()
            .any(|re| re.is_match(&msg.author.name) || re.is_match(&msg.author.id))
        {
            return Verdict::Hide;
        }

        // NGワード: fragment 単位で判定し、境界またぎ誤マッチを防ぐ。
        // emote name は NG テキストに含めない(ユーザーが見るテキスト断片のみ)。
        if self
            .ng_words
            .iter()
            .any(|re| msg.text_fragments().any(|t| re.is_match(t)))
        {
            return Verdict::Hide;
        }

        // ハイライト(テキスト fragment 単位 or 名前にマッチ)。
        if self
            .highlights
            .iter()
            .any(|re| msg.text_fragments().any(|t| re.is_match(t)) || re.is_match(&msg.author.name))
        {
            return Verdict::Highlight;
        }

        Verdict::Show
    }
}

/// 正規表現群をコンパイルする。失敗分は警告して除外。
fn compile_all(patterns: &[String], label: &str) -> Vec<Regex> {
    patterns
        .iter()
        .filter(|p| !p.trim().is_empty())
        .filter_map(|p| match Regex::new(p) {
            Ok(re) => Some(re),
            Err(e) => {
                tracing::warn!("moderation {label} の正規表現が不正: {p:?}: {e}");
                None
            }
        })
        .collect()
}
