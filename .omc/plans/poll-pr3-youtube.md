# アンケート PR3: YouTube 公式投票(2026-11-25、ルート決定。R3 相当: 配信者のチャンネルへ公開の投票を作る・既存 OAuth を使う)

前提:
- PR1(#59、`poll.rs`)と PR2(#60、`poll_control.rs`・`sources/twitch_polls.rs`、SPEC §19.1)はマージ済み。
- 規約は `.claude/CLAUDE.md`、仕様は `.claude/SPEC.md`(§19、§8.1 コメント投稿・OAuth、§7 モデレーション)。
- 受け入れテストは `src-tauri/tests/acceptance/poll_youtube_*.rs` に置き、実装担当は編集しない。テストの誤りはルートへ差し戻す。

## 公式ドキュメントで確認済みの事実(2026-11-25)
- 作成: `POST https://www.googleapis.com/youtube/v3/liveChat/messages?part=snippet`。本文は `snippet.liveChatId`・`snippet.type="pollEvent"`・`snippet.pollDetails.metadata.questionText`・`snippet.pollDetails.metadata.options[{optionText}]`。選択肢は 2〜4 個。コストは 20 units。投票が既に有効なときは 400 `preconditionCheckFailed`(「A pinned active poll already exists.」)。
- 終了: `POST .../liveChat/messages/transition?id=<message id>&status=closed&part=snippet`。本文は付けない。遷移できるのは active から closed だけ。応答の snippet に最終結果が入る。コストは 20 units。
- 集計: `GET .../liveChat/messages?liveChatId=…&part=snippet`(5 units)。応答のトップレベルに `activePollItem`(liveChatMessage リソース)がある。`pollDetails.metadata.options[].tally` は文字列で、チャンネル所有者が認可したリクエストにだけ入る。
- 文字数の上限は記載がない。
- 既存の OAuth スコープは `youtube.force-ssl`(`sources/youtube_send.rs:24`)で足りる。スコープは追加しない。
- liveChatId は OAuth の `liveBroadcasts.list?broadcastStatus=active`(1 unit)で取る既存の経路(`youtube_send.rs` の `live_chat_id`、60 秒キャッシュ)を使う。接続したアカウント自身の配信しか返らない。

## 決めたこと
- 集計は `liveChatMessages.list` の `activePollItem` を 10 秒ごとにポーリングする。ポーリングは YouTube 方式の投票が開いている間だけにする。quota は 1 時間あたり 1,800 units で、配信中の短い投票なら日次の 10,000 で足りる。間隔は定数にする。
- `activePollItem` の id が自分の投票の id と一致する時だけ票を反映する。一致しない、または `activePollItem` が無い時は「YouTube 側で終了された」とみなし、直前の票で closed にする。tally が無い時(所有者でない認可)は、エラー文を出して closed にする。
- 期間は任意。指定があれば(10〜3600 秒、PR1 と同じ範囲)ローカルの期限で transition(closed) を呼び、最終結果を反映する。指定が無ければ手動で終了する。
- 停止は transition(closed)。応答の snippet の tally で最終値を反映する。リセットは、投票がまだ active なら transition(closed) を送ってから idle にする。遠隔の操作が失敗してもローカルは closed / idle にする(PR2 と同じ方針)。
- 作成の再送はしない(20 units)。作成の応答が途切れた時(Network)は、list の `activePollItem` を最大 3 回(1・2・4 秒待って。Network と 5xx の時だけ)確認する。questionText と options が完全一致(trim 後)し、`snippet.publishedAt` が POST 直前の UTC 時刻 − 10 秒より後の時だけ採用する。確認できなければ「YouTube 側に投票が作られた可能性がある」という趣旨のエラーにする。
- 対象: 「自分の配信」(ownStream、enabled)の YouTube 行が 1 件であることを確認する。0 件・2 件以上はそれぞれ別のエラーにする。liveChatId は OAuth の `liveBroadcasts.list` で取る。配信中のものが無い、または複数ある時もエラーにする。
- 文字数の暫定上限: 質問は 1〜100 文字、選択肢は 2〜4 個で各 1〜40 文字(chars で数える)。重複は既存の規則で検査する。YouTube の 400 は理由をエラー文に出す(トークン・URL は除く)。
- Twitch と YouTube の遠隔部分の共通化は、実装担当が判断してよい(trait にするか、分岐にするか)。ただし PR2 の挙動と受け入れテスト `poll_twitch_*` を変えないこと。

## 公開の境界(受け入れテストはここだけを使う)
- `sources::youtube_polls`(新規)
  - `pub struct YoutubePolls` と `pub fn new(base_url: &str) -> Self`。本番は `https://www.googleapis.com/youtube/v3`。テストではモックサーバーを指す。
  - `pub async fn create(&self, token: &str, live_chat_id: &str, question: &str, options: &[String]) -> Result<YtPollCreated, YtPollsError>`。`YtPollCreated { pub id: String, pub published_at_ms: Option<i64> }`。
  - `pub async fn end(&self, token: &str, message_id: &str) -> Result<Option<YtPollState>, YtPollsError>`。transition を送り、応答に snippet があれば状態を返す。
  - `pub async fn active(&self, token: &str, live_chat_id: &str) -> Result<Option<YtPollState>, YtPollsError>`。list の activePollItem を読み、無ければ None。
  - `pub struct YtPollState { pub id: String, pub status: YtPollStatus, pub question: String, pub options: Vec<String>, pub tallies: Option<Vec<u64>>, pub published_at_ms: Option<i64> }`。tallies は、どれか 1 つでも tally が無ければ None。数値にできない tally があっても None。
  - `pub enum YtPollStatus { Active, Closed, Unknown }`。
  - `pub enum YtPollsError { AlreadyActive /*400 preconditionCheckFailed*/, Forbidden(String) /*403 の reason*/, QuotaExceeded /*403 quotaExceeded・dailyLimitExceeded・rateLimitExceeded*/, Unauthorized /*401*/, BadRequest(String) /*その他の 400 の message*/, NotFound /*404*/, Api(u16), Network }`。`Display` は日本語で、トークンと URL を含めない。
  - `pub fn validate_youtube(question: &str, options: &[String]) -> Result<(), &'static str>`。
- `poll`
  - `PollSource::Youtube`(serde では `"youtube"`)。
  - `apply_external` と `set_remote_timing` は source が Chat 以外(Twitch か Youtube)で働く。source が Chat なら false。id の照合は今のまま。
  - source が Youtube の間、`record`(コメント投票)は数えない。
  - `PollStart::validate` は source=Youtube のとき `validate_youtube` の上限で検証し、期間は任意。

## 受け入れ基準(テスト名の粒度)
### HTTP(poll_youtube_http.rs、ローカルのモック)
- `create_posts_poll_event_with_question_and_options`: 本文の snippet.liveChatId・type・questionText・options[].optionText、`part=snippet`、`Authorization: Bearer`。返りは id と publishedAt(RFC 3339 → ms)。
- `end_transitions_to_closed_without_body`: transition の `id` と `status=closed`、`part=snippet`、本文なし。応答の snippet を YtPollState にする。
- `active_reads_active_poll_item_and_tallies`: activePollItem の question、options、tally("12" は 12)、status(active・closed・未知の値)。activePollItem が無ければ None。
- `missing_or_non_numeric_tally_is_none`: tally が 1 つでも無いか、数値でなければ tallies は None。
- `errors_map_by_status_and_reason`: 400 preconditionCheckFailed は AlreadyActive。400 のほかの reason は BadRequest(message)。403 quotaExceeded・dailyLimitExceeded・rateLimitExceeded は QuotaExceeded。403 のほかの reason は Forbidden(reason)。401 は Unauthorized。404 は NotFound。500 は Api(500)。接続拒否は Network。
- `errors_never_contain_token_or_url`: どの YtPollsError の Display にも、渡したトークン・base_url・liveChatId が出ない。
### 検証(poll_youtube_validate.rs)
- `validate_youtube_limits`: 質問 0/1/100/101 文字。選択肢 1/2/4/5 個。各 0/1/40/41 文字。全角と絵文字も chars で数える。trim の後の重複は拒否する。
- `youtube_start_allows_no_duration_and_checks_range`: source=Youtube で duration が None は許す。9 と 3601 は拒否、10 と 3600 は許す。
### 連携(poll_youtube_poll.rs)
- `youtube_poll_ignores_chat_votes`
- `apply_external_and_remote_timing_work_for_youtube_but_not_chat`
- `twitch_behaviour_is_unchanged`: source=Twitch でも、今までどおり apply_external が働くこと(PR2 の境界が壊れていないこと)。

## 実装側の受け入れ基準(レビューで確かめる)
- 開始の順序:
  1. validate
  2. ownStream の YouTube 行が 1 件か
  3. ログインしているか(YouTube の OAuth)
  4. `liveBroadcasts.list` で active な配信が 1 つか
  5. create(再送しない)。応答が途切れたら照合する
  - create が失敗したら idle のままにする。config の保存は成功した後だけ。
- ポーラー: 10 秒ごと。投票 id と CancellationToken に連動させる。HTTP の間はロックを持たない。
  - QuotaExceeded の時は、ポーリングを止めて直前の票で closed にする。日次の上限は時間をおいても戻らないので、文言で知らせる。
  - Unauthorized の時は、既存の `with_token_retry` で 1 回だけ再試行する。
  - Network と 5xx の時は、直前の票を保ったまま次の周期を待つ。
  - エラーは分類名だけを warn に出す(連続する同じ分類は最初の 1 回だけ)。
- UI: Poll.svelte の方式の選択に YouTube を足す。表示するもの:
  - 上限(選択肢 4 個まで)
  - 「期間は任意」
  - quota の注意(10 秒ごとに 5 units)
  - YouTube にログインしていない時の案内(既存のログイン手順へ)
- ログ・エラー・emit にトークン・URL のクエリ・liveChatId を出さない。
- SPEC に §19.2(YouTube 方式)を追加する。§19 の見出しと source の値、§8.1 の liveChatId の用途も更新する。

## 補足(2026-11-25、受け入れテスト作成時の確認)
- `Poll::set_remote_timing` は受け入れテストから呼ぶので `pub` にする。
- `YtPollsError` の Display は、BadRequest を除いて日本語の文言にする。

## 修正3 の裁定(2026-11-25、ルート)
- 期間付き投票の締め切りは、作成の応答(照合で採用した時は採用時)を受けた時点の単調時計から「指定秒数」後とする。YouTube の publishedAt や壁時計からの差し引きはしない。時計のずれで早く閉じることを構造的に無くすため。代わりに、投票が通信の往復分だけ長くなることは許す。
- 残り時間の表示も同じ起点にする(publishedAt は表示に使わない)。設定の保存はこの起点より後でもよいが、起点は保存の前に取る。
