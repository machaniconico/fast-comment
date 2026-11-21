//! 実績解除の TTS 読み上げ文と、読み上げ可否の判定。

use crate::model::{Author, ChatMessage, Fragment, MessageKind, Platform, Roles};

use super::engine::AchievementUnlocked;
use super::model::SubjectKind;
use super::store::AchievementDefs;

/// 読み上げ文。視聴者は `{name}さんが「{実績名}」を獲得`(名前が無ければ「視聴者」)、
/// 配信者は `配信者が「{実績名}」を獲得`。
pub fn announce_text(unlock: &AchievementUnlocked) -> String {
    match unlock.subject.kind {
        SubjectKind::Streamer => format!("配信者が「{}」を獲得", unlock.name),
        SubjectKind::Viewer => {
            let name = unlock
                .subject
                .name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .unwrap_or("視聴者");
            format!("{name}さんが「{}」を獲得", unlock.name)
        }
    }
}

/// 読み上げる解除だけ TTS キュー用のメッセージにする。
///
/// 定義の `announce.tts` が false(定義が無い場合も含む)なら None。視聴者の解除で
/// `is_muted(platform, author_id)` が true(listeners の `tts_muted`)なら None。
/// id は `tts-text-` 始まりにして、コメント用の種別フィルタの対象外(手動テキスト扱い)にする。
pub fn announce_message(
    unlock: &AchievementUnlocked,
    defs: &AchievementDefs,
    is_muted: impl Fn(Platform, &str) -> bool,
) -> Option<ChatMessage> {
    let announce = defs
        .achievements
        .iter()
        .find(|achievement| achievement.id == unlock.achievement_id)
        .is_some_and(|achievement| achievement.announce.tts);
    if !announce {
        return None;
    }
    if unlock.subject.kind == SubjectKind::Viewer {
        if let (Some(platform), Some(author_id)) =
            (unlock.subject.platform, unlock.subject.author_id.as_deref())
        {
            if is_muted(platform, author_id) {
                return None;
            }
        }
    }
    Some(ChatMessage {
        listener: None,
        native_id: false,
        id: format!("tts-text-achievement-{}", unlock.id),
        platform: Platform::Youtube,
        channel: "system".to_string(),
        author: Author {
            nickname: None,
            avatar_url: None,
            id: String::new(),
            name: String::new(),
            display_color: None,
            badges: Vec::new(),
            roles: Roles::default(),
        },
        fragments: vec![Fragment::text(announce_text(unlock))],
        kind: MessageKind::System,
        amount: None,
        timestamp_ms: i64::try_from(unlock.unlocked_at_ms).unwrap_or(i64::MAX),
        raw: None,
        skip_tts: false,
        injected_test: false,
        route: Default::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::achievements::engine::UnlockSubject;
    use crate::achievements::model::Tier;

    fn defs_with(id: &str, tts: bool) -> AchievementDefs {
        let mut defs = AchievementDefs::default();
        for achievement in &mut defs.achievements {
            achievement.announce.tts = achievement.id == id && tts;
        }
        defs
    }

    fn unlock(id: &str, kind: SubjectKind, name: Option<&str>) -> AchievementUnlocked {
        AchievementUnlocked {
            id: "u1".into(),
            achievement_id: id.into(),
            name: "テスト実績".into(),
            description: String::new(),
            tier: Tier::Platinum,
            icon: "🏆".into(),
            subject: UnlockSubject {
                kind,
                platform: Some(Platform::Twitch),
                author_id: Some("a1".into()),
                name: name.map(str::to_owned),
            },
            value_milli: 0,
            threshold_milli: 0,
            currency: None,
            window_key: String::new(),
            unlocked_at_ms: 1,
        }
    }

    fn text(msg: &ChatMessage) -> String {
        msg.fragments
            .iter()
            .map(|fragment| format!("{fragment:?}"))
            .collect()
    }

    #[test]
    fn tts_announce_text_for_viewer_and_streamer() {
        let viewer = unlock("viewer-platinum", SubjectKind::Viewer, Some("たろう"));
        assert_eq!(announce_text(&viewer), "たろうさんが「テスト実績」を獲得");
        let anonymous = unlock("viewer-platinum", SubjectKind::Viewer, None);
        assert_eq!(announce_text(&anonymous), "視聴者さんが「テスト実績」を獲得");
        let streamer = unlock("streamer-platinum", SubjectKind::Streamer, None);
        assert_eq!(announce_text(&streamer), "配信者が「テスト実績」を獲得");

        let defs = defs_with("viewer-platinum", true);
        let msg = announce_message(&viewer, &defs, |_, _| false).unwrap();
        assert!(text(&msg).contains("たろうさんが「テスト実績」を獲得"));
        assert!(msg.id.starts_with("tts-text-"));
    }

    #[test]
    fn tts_announce_skips_when_announce_off() {
        let viewer = unlock("viewer-platinum", SubjectKind::Viewer, Some("たろう"));
        assert!(announce_message(&viewer, &defs_with("viewer-platinum", false), |_, _| false).is_none());
        // 定義に無い実績も読まない。
        assert!(announce_message(&viewer, &defs_with("other", true), |_, _| false).is_none());
    }

    #[test]
    fn tts_announce_respects_listener_mute() {
        let defs = defs_with("viewer-platinum", true);
        let viewer = unlock("viewer-platinum", SubjectKind::Viewer, Some("たろう"));
        let muted = |platform: Platform, author_id: &str| platform == Platform::Twitch && author_id == "a1";
        assert!(announce_message(&viewer, &defs, muted).is_none());
        assert!(announce_message(&viewer, &defs, |_, _| false).is_some());

        // 配信者の解除は視聴者ミュートの対象外。
        let defs = defs_with("streamer-platinum", true);
        let streamer = unlock("streamer-platinum", SubjectKind::Streamer, None);
        assert!(announce_message(&streamer, &defs, |_, _| true).is_some());
    }
}
