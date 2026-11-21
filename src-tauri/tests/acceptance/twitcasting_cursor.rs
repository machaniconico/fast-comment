// registered in main.rs by story TC3-303
//! PR3' 受け入れテスト 4.3: `CommentCursor` / `PollBatch`(純粋、§3 のカーソル規則と §3.1 の固定列 C1〜C5)。
//! ページは応答どおり新しい順。`commit` の出力は古い順。
#![allow(dead_code)]

use fast_comment_lib::sources::twitcasting_api::CommentsQuery;
use fast_comment_lib::sources::twitcasting_official::{CommentCursor, DEDUPE_CAPACITY, MAX_PAGES_PER_POLL};
use serde_json::{json, Value};

fn c(id: &str) -> Value {
    json!({ "id": id, "message": format!("m-{id}") })
}

/// 数値 ID の新しい順ページ(hi → lo)。
fn page(hi: u64, lo: u64) -> Vec<Value> {
    (lo..=hi).rev().map(|i| c(&i.to_string())).collect()
}

fn ids(v: &[Value]) -> Vec<String> {
    v.iter().map(|x| x["id"].as_str().expect("id は文字列").to_string()).collect()
}

fn range_ids(lo: u64, hi: u64) -> Vec<String> {
    (lo..=hi).map(|i| i.to_string()).collect()
}

fn key(q: &CommentsQuery) -> (Option<String>, u32) {
    (q.slice_id.clone(), q.offset)
}

/// 1 回のポーリングを、与えたページ列で最後まで回す。要求した (slice_id, offset) と commit の出力を返す。
/// `pages` を使い切る前に止まる・足りなくて要求が余る場合は失敗にする。
fn poll(cur: &mut CommentCursor, pages: Vec<Vec<Value>>) -> (Vec<(Option<String>, u32)>, Vec<Value>) {
    let mut batch = cur.begin_poll();
    let mut asked = Vec::new();
    let mut pages = pages.into_iter();
    while let Some(q) = batch.next_query() {
        asked.push(key(&q));
        let p = pages.next().unwrap_or_else(|| panic!("ページが足りない: 要求 {asked:?}"));
        batch.push_page(p);
        assert!(asked.len() <= 20, "要求が止まらない");
    }
    assert!(pages.next().is_none(), "要求されなかったページが残った: {asked:?}");
    (asked, cur.commit(batch))
}

/// 確定カーソル 100 の状態を作る(100 の 1 件だけを流し、空ページで止める)。
fn cursor_at_100() -> CommentCursor {
    let mut cur = CommentCursor::new();
    let (_, out) = poll(&mut cur, vec![vec![c("100")], vec![]]);
    assert_eq!(ids(&out), ["100"]);
    assert_eq!(cur.slice_id().as_deref(), Some("100"));
    cur
}

#[test]
fn poll_pages_until_empty_page_not_short_page() {
    assert_eq!(MAX_PAGES_PER_POLL, 5);
    let mut cur = cursor_at_100();
    let mut batch = cur.begin_poll();

    let q = batch.next_query().expect("1 ページ目");
    assert_eq!(key(&q), (Some("100".to_string()), 0));
    batch.push_page(page(170, 121));

    let q = batch.next_query().expect("2 ページ目");
    assert_eq!(key(&q), (Some("100".to_string()), 50));
    batch.push_page(page(120, 101)); // 20 件の短いページでも止めない

    let q = batch.next_query().expect("3 ページ目(短いページの後も取る)");
    assert_eq!(key(&q), (Some("100".to_string()), 100));
    batch.push_page(vec![]);

    assert!(batch.next_query().is_none(), "空ページの後は取らない");
    let out = cur.commit(batch);
    assert_eq!(ids(&out), range_ids(101, 170));
    assert_eq!(cur.slice_id().as_deref(), Some("170"));
}

#[test]
fn poll_stops_after_5_pages() {
    let mut cur = cursor_at_100();
    let mut batch = cur.begin_poll();
    for k in 0..5u64 {
        let q = batch.next_query().unwrap_or_else(|| panic!("{} ページ目が要求されない", k + 1));
        assert_eq!(key(&q), (Some("100".to_string()), (k * 50) as u32));
        // 1 ページ 50 件。全体で 350..101
        batch.push_page(page(350 - k * 50, 301 - k * 50));
    }
    assert!(batch.next_query().is_none(), "5 ページで止まる");
    let out = cur.commit(batch);
    assert_eq!(ids(&out), range_ids(101, 350));
    assert_eq!(cur.slice_id().as_deref(), Some("350"));
}

#[test]
fn uncommitted_batch_leaves_cursor_and_dedupe_untouched() {
    let mut cur = cursor_at_100();
    {
        // C2: 2 ページ目の前で捨てる(エラーで中断した回)
        let mut batch = cur.begin_poll();
        let q = batch.next_query().expect("1 ページ目");
        assert_eq!(key(&q), (Some("100".to_string()), 0));
        batch.push_page(page(170, 121));
        let q = batch.next_query().expect("2 ページ目");
        assert_eq!(key(&q), (Some("100".to_string()), 50));
        // ここで取得失敗: push も commit もせずに捨てる
    }
    assert_eq!(cur.slice_id().as_deref(), Some("100"));
    // 次の回は C1 と同じ。重複集合が汚れていなければ 101..170 が各 1 回出る
    let (asked, out) = poll(&mut cur, vec![page(170, 121), page(120, 101), vec![]]);
    assert_eq!(
        asked,
        vec![(Some("100".to_string()), 0), (Some("100".to_string()), 50), (Some("100".to_string()), 100)]
    );
    assert_eq!(ids(&out), range_ids(101, 170));
    assert_eq!(cur.slice_id().as_deref(), Some("170"));
}

#[test]
fn ids_at_or_below_cursor_are_dropped() {
    // C3: slice_id 以上を返す実装だった場合の応答(100 を含む)
    let mut cur = cursor_at_100();
    let (_, out) = poll(&mut cur, vec![page(110, 100), vec![]]);
    assert_eq!(ids(&out), range_ids(101, 110));
}

#[test]
fn duplicate_ids_across_polls_are_dropped() {
    let mut cur = CommentCursor::new();
    let (_, first) = poll(&mut cur, vec![vec![c("5")], vec![]]);
    assert_eq!(ids(&first), ["5"]);
    let (_, second) = poll(&mut cur, vec![vec![c("6"), c("5")], vec![]]);
    assert_eq!(ids(&second), ["6"]);
}

#[test]
fn numeric_ids_compare_numerically() {
    let mut cur = CommentCursor::new();
    let (_, out) = poll(&mut cur, vec![vec![c("10"), c("9")], vec![]]);
    assert_eq!(ids(&out), ["9", "10"]);
    assert_eq!(cur.slice_id().as_deref(), Some("10"));
}

#[test]
fn non_numeric_ids_never_move_cursor_and_omit_slice_id() {
    // C5
    let mut cur = CommentCursor::new();
    assert_eq!(cur.slice_id(), None);
    let (asked, out) = poll(&mut cur, vec![vec![c("b"), c("a")], vec![]]);
    assert_eq!(asked, vec![(None, 0), (None, 50)]);
    assert_eq!(ids(&out), ["a", "b"]);
    assert_eq!(cur.slice_id(), None);

    let (asked, out) = poll(&mut cur, vec![vec![c("c"), c("b"), c("a")], vec![]]);
    assert_eq!(asked, vec![(None, 0), (None, 50)]);
    assert_eq!(ids(&out), ["c"]);
    assert_eq!(cur.slice_id(), None);
}

#[test]
fn mid_stream_start_returns_newest_20() {
    let mut cur = CommentCursor::new();
    let mut batch = cur.begin_poll();
    let q = batch.next_query().expect("途中参加は 1 ページだけ取る");
    assert_eq!(key(&q), (None, 0));
    batch.push_page(page(150, 101));
    let out = cur.commit_mid_stream(batch);
    assert_eq!(ids(&out), range_ids(131, 150));
    assert_eq!(cur.slice_id().as_deref(), Some("150"));
}

#[test]
fn reset_for_new_movie_clears_cursor_and_dedupe() {
    let mut cur = cursor_at_100();
    cur.reset();
    assert_eq!(cur.slice_id(), None);
    let (asked, out) = poll(&mut cur, vec![vec![c("100")], vec![]]);
    assert_eq!(asked, vec![(None, 0), (None, 50)]);
    assert_eq!(ids(&out), ["100"], "旧 ID が再び通る");
}

#[test]
fn dedupe_capacity_evicts_oldest_non_numeric() {
    assert_eq!(DEDUPE_CAPACITY, 4096);
    let mut cur = CommentCursor::new();
    // 非数値 ID n0..n4199 を 1 件ずつ別の poll で確定する(4200 件 > 容量 4096)
    for i in 0..4200 {
        let (_, out) = poll(&mut cur, vec![vec![c(&format!("n{i}"))], vec![]]);
        assert_eq!(out.len(), 1, "n{i} は初出なので通る");
    }
    assert_eq!(cur.slice_id(), None);
    // 保持中(直近 4096 件 = n104..n4199)の再送は落ちる
    let (_, out) = poll(&mut cur, vec![vec![c("n104")], vec![]]);
    assert!(out.is_empty(), "n104 は保持中のはず");
    let (_, out) = poll(&mut cur, vec![vec![c("n4199")], vec![]]);
    assert!(out.is_empty(), "n4199 は保持中のはず");
    // 追い出し済みの最古の再送は通る(最後に確かめる: 通すと別の ID が追い出される)
    let (_, out) = poll(&mut cur, vec![vec![c("n0")], vec![]]);
    assert_eq!(ids(&out), ["n0"], "n0 は追い出し済みのはず");
}
