// registered in main.rs by story TC3-303
// twitcasting_common.rs から公式 Source 依存の補助だけを分離(TC3-301 の時点で common をコンパイルできるようにするため。ルート 2026-11-21)
use fast_comment_lib::integrations::MemorySecretStore;
use fast_comment_lib::sources::twitcasting_official::{PollTuning, TwitcastingOfficialSource};
use crate::twitcasting_common::*;

pub fn tuning() -> PollTuning {
    PollTuning { offline: ms(50), live_refresh: ms(50), comment_unit: ms(20), not_found_retry: ms(200), jitter: false }
}

pub fn official_source(rig: &Rig, id: &str, chan: &Chan, t: PollTuning) -> TwitcastingOfficialSource<MemorySecretStore> {
    TwitcastingOfficialSource::new(id.to_string(), rig.api.clone(), rig.auth.subscribe(), Some(chan.meta_tx()), t)
        .with_live_tx(Some(chan.live_tx()))
}

