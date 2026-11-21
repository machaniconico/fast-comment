// registered in main.rs by story TC3-304 (needs TwitcastingRoutedSource; the channel_name test alone could go in with TC3-302)
//! PR3' 受け入れテスト 4.7(B2): 解釈できない識別子(URL やトークンを貼られた場合)をログ・channel・notice・
//! `Source::name()` に生で出さない。偽サーバの受信は 0 件のままであること。
//!
//! `format!("{:?}", source)` の検査は、契約が Source に Debug を求めていないため行わない(コンパイル不能になりうる)。
#![allow(dead_code)]

use crate::twitcasting_common::*;
use crate::twitcasting_common_official::*;
use fast_comment_lib::config::TwitcastingMode;
use fast_comment_lib::sources::twitcasting::{channel_name, INVALID_CHANNEL_LABEL, INVALID_ID_NOTICE};
use fast_comment_lib::sources::twitcasting_mode::TwitcastingRoutedSource;
use fast_comment_lib::sources::Source;

const ID: &str = "https://invalid.example/?token=MARKER";
const LEAKS: [&str; 2] = ["MARKER", "invalid.example"];

#[tokio::test(flavor = "current_thread")]
async fn invalid_identifier_never_leaks_marker() {
    let (logs, _guard) = capture_logs();
    let r = rig_logged_in().await;
    logs.clear();
    Script::new().install(&r.fake);
    let site = FakeApi::start().await;
    let mut chans: Vec<Chan> = Vec::new();

    // TwitcastingSource 単体
    {
        let chan = Chan::new();
        let src = unofficial_source(ID, &site.base, &chan);
        assert_absent("Source::name()", &src.name(), &LEAKS);
        let run = chan.run(src);
        settle(200).await;
        run.stop().await;
        chans.push(chan);
    }
    // TwitcastingRoutedSource(3 モードそれぞれ)
    for mode in [TwitcastingMode::Auto, TwitcastingMode::Unofficial, TwitcastingMode::Official] {
        let chan = Chan::new();
        let src = TwitcastingRoutedSource::new(
            mode,
            unofficial_source(ID, &site.base, &chan),
            official_source(&r, ID, &chan, tuning()),
            r.auth.subscribe(),
            Some(chan.meta_tx()),
        );
        assert_absent("Source::name()", &src.name(), &LEAKS);
        let run = chan.run(src);
        settle(200).await;
        run.stop().await;
        chans.push(chan);
    }

    for (i, chan) in chans.iter().enumerate() {
        let metas = chan.metas();
        assert_absent("YoutubeMetadataUpdate の Debug", &format!("{metas:?}"), &LEAKS);
        assert!(
            metas.iter().any(|m| m.notice.as_deref() == Some(INVALID_ID_NOTICE)),
            "{i} 番目の Source が INVALID_ID_NOTICE を出さない: {metas:?}"
        );
        assert!(
            metas.iter().all(|m| m.channel == INVALID_CHANNEL_LABEL),
            "{i} 番目の Source の channel が固定名ではない: {metas:?}"
        );
    }
    assert_absent("ログ", &logs.text(), &LEAKS);
    assert_eq!(site.total(), 0, "偽サイトに要求が届いた");
    assert_eq!(r.fake.total(), 0, "偽 API に要求が届いた");
}

#[test]
fn channel_name_of_invalid_identifier_is_fixed_label() {
    assert_eq!(channel_name("https://invalid.example/?token=MARKER"), INVALID_CHANNEL_LABEL);
    assert_eq!(channel_name("x:bad"), INVALID_CHANNEL_LABEL);
    assert_eq!(INVALID_CHANNEL_LABEL, "twitcasting-invalid");
}
