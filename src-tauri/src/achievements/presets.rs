use super::model::{
    Achievement, Announce, Condition, CurrencyMode, Metric, SubjectKind, Tier, Window,
};

pub fn presets() -> Vec<Achievement> {
    use Metric::*;
    use SubjectKind::*;
    use Tier::*;
    use Window::*;

    let mut presets = [
        (
            "viewer-first-comment",
            "初コメント",
            Viewer,
            Bronze,
            Lifetime,
            Comments,
            1.0,
        ),
        (
            "viewer-comments-100",
            "コメント100件",
            Viewer,
            Silver,
            Lifetime,
            Comments,
            100.0,
        ),
        (
            "viewer-comments-1000",
            "コメント1000件",
            Viewer,
            Gold,
            Lifetime,
            Comments,
            1000.0,
        ),
        (
            "viewer-first-superchat",
            "初スパチャ",
            Viewer,
            Bronze,
            Lifetime,
            SuperChatCount,
            1.0,
        ),
        (
            "viewer-donation-10000",
            "累計1万円",
            Viewer,
            Gold,
            Lifetime,
            DonationAmount,
            10_000.0,
        ),
        (
            "viewer-first-membership",
            "メンバー加入",
            Viewer,
            Silver,
            Lifetime,
            MembershipEvents,
            1.0,
        ),
        (
            "viewer-streams-attended-5",
            "5配信に参加",
            Viewer,
            Silver,
            Lifetime,
            StreamsAttended,
            5.0,
        ),
        (
            "streamer-per-stream-comments-1000",
            "1配信1000コメント",
            Streamer,
            Bronze,
            PerStream,
            Comments,
            1000.0,
        ),
        (
            "streamer-comments-10000",
            "累計1万コメント",
            Streamer,
            Silver,
            Lifetime,
            Comments,
            10_000.0,
        ),
        (
            "streamer-peak-viewers-100",
            "同時視聴100人",
            Streamer,
            Silver,
            PerStream,
            PeakConcurrentViewers,
            100.0,
        ),
        (
            "streamer-donation-100000",
            "累計10万円",
            Streamer,
            Gold,
            Lifetime,
            DonationAmount,
            100_000.0,
        ),
    ]
    .into_iter()
    .map(|(id, name, subject, tier, window, metric, threshold)| {
        let condition = Condition {
            window,
            metric,
            threshold,
            currency: if metric == DonationAmount {
                CurrencyMode::JpyConverted
            } else {
                CurrencyMode::None
            },
            platforms: Vec::new(),
        };
        preset(id, name, subject, tier, Some(condition))
    })
    .collect::<Vec<_>>();

    presets.extend([
        preset("viewer-platinum", "視聴者プラチナ", Viewer, Platinum, None),
        preset(
            "streamer-platinum",
            "配信者プラチナ",
            Streamer,
            Platinum,
            None,
        ),
    ]);
    presets
}

fn preset(
    id: &str,
    name: &str,
    subject: SubjectKind,
    tier: Tier,
    condition: Option<Condition>,
) -> Achievement {
    let icon = match tier {
        Tier::Bronze => "🥉",
        Tier::Silver => "🥈",
        Tier::Gold => "🥇",
        Tier::Platinum => "🏆",
    };
    Achievement {
        id: id.to_owned(),
        name: name.to_owned(),
        description: String::new(),
        icon: icon.to_owned(),
        tier,
        subject,
        enabled: true,
        builtin_id: Some(id.to_owned()),
        edited: false,
        condition,
        announce: Announce::default(),
    }
}
