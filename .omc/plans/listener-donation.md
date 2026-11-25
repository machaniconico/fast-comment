# ユーザー別ギフト金額(2026-11-25、ルート決定。R3: listeners.json の永続フォーマットに欄を足す)

前提: 規約は `.claude/CLAUDE.md`、仕様は `.claude/SPEC.md`(§3 Amount、§8 UI、§17 実績)。受け入れテストは `src-tauri/tests/acceptance/listener_donation*.rs` に置く。実装担当はこのテストを編集しない。

## 決めたこと
- リスナーごとに、累計を通貨別に持つ。換算した値は保存しない。保存先は既存の `listeners.json`。
  - `ListenerRecord` に `donation: BTreeMap<String, i64>`(serde 名は `donation`)を足す。値はミリ単位の整数(value × 1000 を四捨五入)。
  - 欄が無い古い `listeners.json` も読める(既定は空)。読んだあと保存しても、ほかの欄は失われない。
- 加算は `ListenerStore::observe` で行う。条件は 3 つすべて。
  - `msg.amount` が Some
  - kind が SuperChat、Bits、Gift のどれか
  - 通貨が `SUBGIFT` でない(サブギフトは件数なので除く。実績エンジンと同じ規則)
  - 値が有限で 0 より大きい。NaN、無限大、0 以下は数えない。
  - 加算は飽和加算にする(オーバーフローで負にしない)。
  - 通貨のキーは `achievements::currency::normalize_currency(raw, &BTreeMap::new())` を通した値にする(ISO の大文字コード、記号は既知の表で寄せる)。`BITS` と `pt` は独立の通貨として扱う。
- 実績機能の有効・無効に関係なく集計する(実績エンジンには依存しない)。
- リセット:
  - `ListenerStore::reset_donation(platform, author_id) -> bool`: その人の donation を空にする。存在しなければ false。
  - `ListenerStore::reset_all_donations() -> usize`: 全員の donation を空にし、空にした人数を返す。
  - リスナーを削除すると、donation も一緒に消える(既存の delete)。
- 配信ごとの集計はこの PR では作らない。範囲は累計だけ。
- 換算した目安(円)は表示の時だけ計算する。Rust 側で、実績のレート表(`achievements.json` の rates と symbolOverrides)と `AmountTotals::jpy_milli` を使う。レートが無い通貨は目安から除き、その通貨の名前を「換算できない通貨」として返す。
- UI:
  - リスナー詳細(`UserCard.svelte`)の「累計ギフト」の近くに、通貨別の合計を並べる(例: `¥12,000` / `USD 5.00` / `Bits 300` / `pt 1,500`)。並びは額の大きい順で、上限は 6 通貨。7 通貨目からは「ほか N 通貨」にまとめる。
  - 円の目安は 1 行で出す。換算できない通貨があれば、その旨も添える。
  - 「金額をリセット」ボタン(その人だけ、確認つき)を置く。全員のリセットは設定のリスナー管理に置く。リスナー管理が無ければ、ツールの実績画面の近くで構わない。
  - 金額の整形は既存の表示関数があれば再利用する。
- 新しい IPC:
  - `listener_donation(platform, authorId) -> { totals: [{currency, milli}], jpyEstimateMilli: Option<i64>, unconvertible: [currency] }`
  - `reset_listener_donation(platform, authorId)`
  - `reset_all_listener_donations()`
  - ローカル API の読み取り(`/api/listeners/...` に既存の口があれば)にも donation を載せる。トークンなど秘密を含めない。
- 履歴の再送(`msg.route.uncounted == true`)は数えない(実績エンジンと同じ。2026-11-25 レビューで追加)。
- 既知の限界(SPEC に書く): uncounted の目印が無い経路で同じコメントが二重に届いた場合は、二重に数える場合がある。ツイキャスのギフトと X には金額が無いので数えない。

## 公開の境界(受け入れテストはここだけを使う)
- `listeners::{ListenerStore, ListenerRecord}`
  - `ListenerStore::load(dir)`
  - `observe(&mut ChatMessage)`
  - `get(platform, author_id)`
  - `delete(platform, author_id)`
  - `flush()`
  - `reset_donation(platform, author_id) -> bool`
  - `reset_all_donations() -> usize`
- `ListenerRecord.donation: BTreeMap<String, i64>`
- `model::{ChatMessage, Amount, MessageKind}`(既存。テストで組み立てる)
- `listeners::donation_summary(donation: &BTreeMap<String, i64>, rates: &RateTable, overrides: &BTreeMap<String, String>) -> DonationSummary`(新規。pub)
  - `DonationSummary { totals: Vec<(String, i64)> /* 額の大きい順。同額なら通貨コード順 */, jpy_estimate_milli: Option<i64> /* 換算できる通貨が 1 つも無ければ None */, unconvertible: Vec<String> /* 通貨コード順 */ }`
  - `RateTable` は `achievements::currency` の既存の型を使う。テストで組めないなら、実装担当が pub にする。

## 受け入れ基準(テスト名の粒度、listener_donation.rs)
- `super_chat_bits_and_gift_amounts_accumulate_per_currency`: JPY 500 の SuperChat と JPY 1000 の SuperChat で `JPY=1_500_000`。Bits 100 で `BITS=100_000`。USD 4.99 の Gift で `USD=4_990`。
- `subgift_membership_normal_and_missing_amount_are_ignored`: 次は donation に入らない。通貨 `SUBGIFT` の Gift、Membership(金額つき)、Normal(金額つき)、`amount=None` の Gift。
- `non_finite_zero_or_negative_values_are_ignored`: NaN、無限大、0、-5 は数えない。
- `currency_symbol_is_normalized`: 通貨が `¥` なら `JPY`、`$` なら既存の表どおり(実装の表の値と同じになることを確認)。前後の空白付きの `" jpy "` は `JPY`。
- `saturating_add_never_wraps`: i64 の上限付近から加算しても負にならない。
- `old_listeners_json_without_donation_loads_and_keeps_fields`: donation の無い JSON を load し、observe なしで flush して読み直す。ほかの欄(nickname・memo・commentCount・ttsMuted など)が保たれ、donation が空であること。
- `donation_persists_across_flush_and_reload`
- `reset_donation_clears_one_listener_only`、`reset_donation_unknown_listener_is_false`
- `reset_all_donations_returns_cleared_count`(donation が空の人は数えない)
- `delete_removes_donation`
- `summary_orders_by_amount_and_estimates_jpy`: JPY 1000、USD 10(レートは 150)、BITS 500(レートなし)の場合。totals は額の大きい順。jpy_estimate_milli は 1000 + 1500 = 2500 円をミリにした値。unconvertible は `["BITS"]`。
- `summary_without_any_convertible_currency_is_none`

## 実装側の受け入れ基準(レビューで確かめる)
- `observe` の加算でロックの範囲を広げない。保存の頻度は既存の flush に従う。
- UI は描画だけにする。整形の数値は Rust が返す milli を表示用に割るだけにとどめる。
- ログやエラーに、リスナーの名前・メモを新たに出さない。
- SPEC を更新する。§8 のリスナー詳細、§10 の listeners.json の欄、既知の限界。
- pnpm test と pnpm check を通す。

## 修正1 の裁定(2026-11-25、レビュー後)
- 保存時の通貨キーは、その時点のユーザーの symbolOverrides(achievements.json)を通して正規化する(上書きが空なら従来どおり。受け入れテストの期待は変わらない)。後から上書きを変えても過去の分は付け替えない(既知の限界として SPEC に書く)。
- `route.uncounted` の再送は数えない。
- donation の読み込みは寛容に: 不正な項目(数値でない・i64 外・負)だけ捨て、ファイル全体は退避させない。
- レート 0 以下の通貨は換算できない扱い。円の目安は整数円に丸めて表示する。
