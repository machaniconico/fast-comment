# 既知の小さな不具合 4 件(fix/minor-followups)

仕様の正本は `.claude/SPEC.md`(§5・§19.1・§19.2)。ここには、各件が再現したか、何を選んだか、その理由だけを残す。

## 1. YouTube 公式投票(PR #61)

### (a) 開いた直後に 0 票のスナップショットが出る

- 再現: した。`YoutubePollControl::start` が `Poll::open`(0 票で配信)→ `apply_external`(票数で配信)→ `set_remote_timing`(締め切りで配信)と 3 回配信していた。作成応答に A=7・B=3 がある時、1 フレーム目は 0 票(両方が `leading`)。
- 対応: `Poll::open_external(start, votes, timing)` を足し、採用済みの票数と開始・終了予定時刻を開く時の 1 回の配信に載せる。`Poll::open` は `open_external(start, None, None)`。票数の書き込みは `apply_external` と共通の `set_votes`。配信は `publish` 1 回なので、画面(`emit`)と OBS(`watch`)の両方で 0 票のフレームが無くなる。
- Twitch 方式の `open` → `set_remote_timing`(照合で採用した時だけ 2 回配信)は触っていない。票数は 0 のままなので 0 票の問題は無い。
- テスト: `poll_control_youtube::tests::open_publishes_adopted_tallies_and_timing_in_one_frame`(配信したスナップショットが 1 件で、戻り値と一致し、7・3 と締め切りを持つ)。

### (b) 作成応答を失った後の照合が壁時計に依存する

- 再現: した。旧条件は `publishedAt > POST 直前の PC の UTC − 10 秒`。
  - PC の時計が 60 秒進んでいる時: 自分が作った投票を見落として `CreateUnconfirmed`(投票は YouTube に残る)。
  - PC の時計が 60 秒遅れている時: 30 秒前からある同じ内容の投票を採用する(古い票数を引き継ぐ)。
- 検討した条件:
  - 質問・選択肢の完全一致 + 受付中だけ(時刻を見ない): 設定に残った同じ質問で作り直す場面で、前回の終了に失敗して残った投票を古い票数ごと採用しうる。既存の `stale_match_does_not_open_or_save_preferences` が守っている挙動を捨てることになるので採らなかった。
  - 単調時計だけ: YouTube の時刻と座標系が違うので比べられない。
  - 採用: 公開時刻を、同じ list 応答の `Date` ヘッダ(YouTube 側の時計)と比べる。PC の時計は POST を送ってからの経過(単調時計の差)にだけ使う。`publishedAt > Date + 1 秒 − 経過 − 10 秒`。`Date` は秒に切り捨てられているので 1 秒進める(窓は 9〜10 秒。自分の投票は POST より後に公開されるので 9 秒以上の余裕がある)。
- `Date` が欠落・不正な時は PC の時計で代用する(従来と同じ条件になる)。YouTube のパースは欠落で劣化させる、という制約に合わせた。
- 質問・選択肢の完全一致と受付中の確認は従来どおり。
- テスト: `reconcile_adopts_own_poll_when_pc_clock_is_ahead`、`reconcile_rejects_older_poll_when_pc_clock_is_behind`、`reconcile_uses_pc_clock_when_date_header_is_unusable`。既存の `stale_match_does_not_open_or_save_preferences` はそのまま通る。

## 2. リスナーごとのギフト金額(PR #62)

### (a) `author.id` が空だとカードの金額がその場で更新されない

- 再現: した。台帳(`listeners.rs` の `observe_at`)は著者 ID が空のコメントを名前で数える。カードの `authorId` もその名前(設定のリスナー一覧から開く。コメントからは著者 ID が空だと開けない)。一方 `UserCard.svelte` のバッチ購読は `msg.author.id === t.authorId` で比べていたので、空の ID のギフトでは再取得しなかった。
- 対応: 判定を `types.ts` の `hasDonationFrom(messages, platform, authorId)` に出し、著者 ID が空なら名前で照合する(`listeners.rs` と同じく trim しない)。集計そのものは従来どおり Rust。
- 触っていないもの: カードのセッション内コメント一覧(`sessionCommentsOf`)とモデレーションの対象も `author.id` の一致で、空の ID では同じずれがある。依頼の範囲外。
- テスト: `src/lib/listenerDonation.test.ts`(`package.json` の `test` に追加)。

### (b) `lib.rs:119` 付近の古いコメント

- PR #62 の時点の 119 行目は `achievement_defs` の「定義の保存 IPC は PR3 で足す。それまでは読まない」と `#[allow(dead_code)]`。今は実績の IPC、ギフト金額の集計、効果音の換算が読んでいる。
- 対応: コメントを実態に合わせ、不要になった `#[allow(dead_code)]` を外した(警告は出ない)。

## 3. Twitch 公式 Polls の確認待ち(PR #60)

- 再現: した。照合 GET に応答が無いと、GET ごとに HTTP のタイムアウト 15 秒を待ち、1+15+2+15+4+15 = 52 秒(作成 POST の 15 秒と合わせて約 67 秒)。
- 対応: 照合は作成の失敗から 10 秒で打ち切る(`MATCH_TIMEOUT`)。GET は失敗から 1・3・7 秒後に送り、応答を待つのは次の GET の時刻まで(最後は 10 秒まで)。応答が無いまま待ち切った GET は通信失敗と同じく次を試す。
- 重複作成の回避は変えていない。作成 POST は送り直さず、確認できなければ従来の「作られた可能性があります」のエラー(`CreateUnconfirmed`)。
- 作成 POST 自体のタイムアウト(15 秒)は変えていない。縮めると「作成できたか分からない」場面が増えるため。最悪は 15 + 10 = 25 秒。
- 触っていないもの: YouTube 方式の照合も同じ形(GET ごとに 15 秒)で、最長 52 秒待ちうる。Twitch 方式の `started_at` と PC の壁時計の比較(1(b) と同じ種類の時計ずれ)も残っている。どちらも依頼の範囲外。
- テスト: `poll_control::tests::create_match_gives_up_10s_after_create_failure_without_resending_post`(POST 1 回、GET 3 回、10〜11.5 秒で `CreateUnconfirmed`)。既存の `create_match_get_retries_only_network_and_5xx_with_backoff` の間隔はそのまま通る。

## 4. OBS ポートの競合

- 再現: コードの読みで確認(保存経路は `AppHandle` が要るので単体テストから呼べない)。`save_config_update` は `commit_config` で新ポートを保存した後に bind していた。bind に失敗すると `?` で抜け、保存値は新ポート・待受は旧ポートになり、その後のチャンネル差分の適用なども飛んでいた。
- 選んだ規則: **旧ポートのまま保存し、失敗を UI に知らせる**。
  - 保存の前に新ポートを bind する。bind できたら保存の確定後に旧サーバを止めて切り替える。保存に失敗したら新サーバを止める(`PendingObsServer` を捨てると止まる)。
  - bind できなければ `obs.port` だけを今の待受ポートに戻して保存し、`warnings` で知らせる。UI は応答の設定でフォームを置き換えるので、ポート欄も今の値に戻る。UI の変更は無い。
- 理由:
  - 保存した値と待受ポートが常に一致する(「保存するが旧ポートで待受中と知らせる」案は食い違いが残り、次回起動で新ポートも開けなければ OBS 用サーバが無くなる)。
  - 「何も保存しない」案は、設定の取り込みが手元のポート競合だけで全体失敗する。取り込みには、選べない値だけ今の値に残して知らせる前例がある(`settle_x_post_mode`)。通常の保存と取り込みを同じ規則にできる。
  - 他の項目の変更を失わない。
- 警告に載せるのはポート番号だけ(トークン・URL は出さない)。
- 起動時に bind できない場合の挙動は変えていない(ログに出すだけで、待受が無いまま動く)。その状態でも嘘にならないよう、警告は「待ち受けている」とは書かず「変更せず、今の値のまま保存した」と書く。
- テスト: `obs_port_conflict_keeps_listening_port_and_warns`(実際に使用中のポートで bind を失敗させる)、`obs_port_change_binds_before_save_and_same_port_is_left_alone`、`pending_obs_server_stops_unless_switched`。
