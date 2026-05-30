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

#[cfg(test)]
mod tests {
    use super::{Moderator, Verdict};
    use crate::config::ModerationConfig;
    use crate::model::{Author, ChatMessage, Fragment, MessageKind, Platform, Roles};

    fn config(
        ng_words: &[&str],
        ng_users: &[&str],
        highlights: &[&str],
    ) -> ModerationConfig {
        ModerationConfig {
            ng_words: ng_words.iter().map(|s| (*s).to_string()).collect(),
            ng_users: ng_users.iter().map(|s| (*s).to_string()).collect(),
            highlights: highlights.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    fn message(id: &str, author_id: &str, author_name: &str, texts: &[&str]) -> ChatMessage {
        ChatMessage {
            id: id.to_string(),
            platform: Platform::Twitch,
            channel: "test-channel".to_string(),
            author: Author {
                id: author_id.to_string(),
                name: author_name.to_string(),
                display_color: None,
                badges: Vec::new(),
                roles: Roles::default(),
            },
            fragments: texts.iter().map(|text| Fragment::text(*text)).collect(),
            kind: MessageKind::Normal,
            amount: None,
            timestamp_ms: 0,
            raw: None,
            skip_tts: false,
        }
    }

    #[test]
    fn ng_word_regex_match_hides_message() {
        // NGワードは正規表現として本文Text断片にマッチしたら非表示判定になる。
        let moderator = Moderator::new(&config(&[r"b[a@]d\s+word", "禁止"], &[], &[]));

        assert_eq!(
            moderator.judge(&message("m1", "u1", "viewer", &["this is b@d word"])),
            Verdict::Hide
        );
        assert_eq!(
            moderator.judge(&message("m2", "u2", "viewer", &["これは禁止です"])),
            Verdict::Hide
        );
    }

    #[test]
    fn ng_user_regex_match_hides_message() {
        // NGユーザーは著者名または著者IDに正規表現がマッチしたら非表示判定になる。
        let moderator = Moderator::new(&config(&[], &[r"^bad_(name|id)$"], &[]));

        assert_eq!(
            moderator.judge(&message("m1", "u1", "bad_name", &["hello"])),
            Verdict::Hide
        );
        assert_eq!(
            moderator.judge(&message("m2", "bad_id", "viewer", &["hello"])),
            Verdict::Hide
        );
    }

    #[test]
    fn highlight_rules_for_user_and_keyword_mark_highlight() {
        // ハイライトルールは本文キーワードまたは著者名にマッチしたらHighlight判定になる。
        let moderator = Moderator::new(&config(&[], &[], &[r"\burgent\b", r"^featured_user$"]));

        assert_eq!(
            moderator.judge(&message("m1", "u1", "viewer", &["needs urgent review"])),
            Verdict::Highlight
        );
        assert_eq!(
            moderator.judge(&message("m2", "u2", "featured_user", &["ordinary text"])),
            Verdict::Highlight
        );
    }

    #[test]
    fn hidden_id_hides_message_and_unhide_restores_judgement() {
        // 手動のローカル非表示IDは設定ルールに関係なく最優先で非表示判定になる。
        let mut moderator = Moderator::new(&ModerationConfig::default());
        let msg = message("hidden-message", "u1", "viewer", &["hello"]);

        assert_eq!(moderator.judge(&msg), Verdict::Show);
        moderator.hide_id("hidden-message");
        assert_eq!(moderator.judge(&msg), Verdict::Hide);
        moderator.unhide_id("hidden-message");
        assert_eq!(moderator.judge(&msg), Verdict::Show);
    }

    #[test]
    fn invalid_regex_patterns_are_skipped_without_panic() {
        // 不正な正規表現はコンパイル時にスキップされ、判定実行時もpanicしない。
        let result = std::panic::catch_unwind(|| {
            let mut moderator = Moderator::new(&config(&["["], &["("], &["*bad"]));
            let msg = message("m1", "u1", "viewer", &["plain text"]);

            assert_eq!(moderator.judge(&msg), Verdict::Show);

            moderator.update_config(&config(&["[invalid", r"safe\s+word"], &[], &[]));
            assert_eq!(
                moderator.judge(&message("m2", "u2", "viewer", &["safe word"])),
                Verdict::Hide
            );
        });

        assert!(result.is_ok());
    }
}
