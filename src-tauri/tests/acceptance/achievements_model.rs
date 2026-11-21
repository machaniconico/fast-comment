// registered in main.rs by story ACH-001
//! 実績の条件モデル・検証・プリセット(acceptance.md §3.1)。
use crate::achievements_common::*;
use fast_comment_lib::achievements::model::{
    duplicate, presets, reset_to_preset, validate_all, Achievement, CurrencyMode, Metric, SubjectKind, Tier,
    ValidationError, Window,
};
use fast_comment_lib::model::Platform;
use serde_json::json;

fn viewer_ok(id: &str) -> Achievement {
    viewer_count(id, Metric::Comments, 1.0)
}

/// 検証結果の (id, 種類) を取り出す。Ok なら None。
fn verdict(list: &[Achievement]) -> Option<(String, ValidationError)> {
    validate_all(list).err()
}

fn assert_err(list: &[Achievement], want_id: &str, want: impl Fn(&ValidationError) -> bool, what: &str) {
    match verdict(list) {
        Some((id, e)) => {
            assert_eq!(id, want_id, "{what}: 対象の id");
            assert!(want(&e), "{what}: 別のエラーだった");
        }
        None => panic!("{what}: 受理されてしまった"),
    }
}

#[test]
fn presets_all_pass_validation() {
    let list = presets();
    assert!(!list.is_empty());
    assert!(validate_all(&list).is_ok());
}

#[test]
fn presets_include_viewer_and_streamer_platinum() {
    let list = presets();
    for kind in [SubjectKind::Viewer, SubjectKind::Streamer] {
        let want = to_json(&kind);
        let plats: Vec<&Achievement> = list
            .iter()
            .filter(|a| matches!(a.tier, Tier::Platinum) && to_json(&a.subject) == want)
            .collect();
        assert_eq!(plats.len(), 1, "{want} のプラチナはちょうど 1 つ");
        assert!(plats[0].condition.is_none(), "プラチナに条件は無い");
    }
    assert!(list.iter().all(|a| a.builtin_id.is_some()), "プリセットは builtin_id を持つ");
}

#[test]
fn validate_rejects_amount_metric_without_currency() {
    let a = make("a", SubjectKind::Viewer, Tier::Bronze, Some(cond(Window::Lifetime, Metric::DonationAmount, 100.0, CurrencyMode::None)));
    assert_err(&[a], "a", |e| matches!(e, ValidationError::AmountMetricNeedsCurrency), "金額系で通貨なし");
}

#[test]
fn validate_rejects_count_metric_with_currency() {
    for c in [CurrencyMode::JpyConverted, CurrencyMode::Only("USD".to_string())] {
        let a = make("a", SubjectKind::Viewer, Tier::Bronze, Some(cond(Window::Lifetime, Metric::Comments, 5.0, c)));
        assert_err(&[a], "a", |e| matches!(e, ValidationError::CountMetricRejectsCurrency), "回数系で通貨あり");
    }
}

#[test]
fn validate_rejects_streamer_metric_on_viewer() {
    let a = make("a", SubjectKind::Viewer, Tier::Bronze, Some(cond(Window::PerStream, Metric::UniqueCommenters, 5.0, CurrencyMode::None)));
    assert_err(&[a], "a", |e| matches!(e, ValidationError::MetricSubjectMismatch), "視聴者に UniqueCommenters");
    let b = viewer_count("b", Metric::Streams, 5.0);
    assert_err(&[b], "b", |e| matches!(e, ValidationError::MetricSubjectMismatch), "視聴者に Streams");
    let c = make("c", SubjectKind::Streamer, Tier::Bronze, Some(cond(Window::Lifetime, Metric::StreamsAttended, 5.0, CurrencyMode::None)));
    assert_err(&[c], "c", |e| matches!(e, ValidationError::MetricSubjectMismatch), "配信者に StreamsAttended");
}

#[test]
fn validate_rejects_peak_viewers_lifetime() {
    let a = streamer_count("a", Window::Lifetime, Metric::PeakConcurrentViewers, 100.0);
    assert_err(&[a], "a", |e| matches!(e, ValidationError::WindowNotAllowed), "ピーク同時視聴の Lifetime");
}

#[test]
fn validate_rejects_nonpositive_nan_or_infinite_threshold() {
    for t in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        let a = viewer_count("a", Metric::Comments, t);
        assert_err(&[a], "a", |e| matches!(e, ValidationError::BadThreshold), "しきい値");
    }
}

#[test]
fn validate_rejects_platinum_with_condition() {
    let mut p = platinum("p", SubjectKind::Viewer);
    p.condition = Some(cond(Window::Lifetime, Metric::Comments, 5.0, CurrencyMode::None));
    assert_err(&[p], "p", |e| matches!(e, ValidationError::PlatinumHasCondition), "条件つきプラチナ");
}

#[test]
fn validate_rejects_non_platinum_without_condition() {
    let a = make("a", SubjectKind::Viewer, Tier::Gold, None);
    assert_err(&[a], "a", |e| matches!(e, ValidationError::MissingCondition), "条件なしの非プラチナ");
}

#[test]
fn validate_rejects_second_platinum_same_subject() {
    let two_viewers = [platinum("p1", SubjectKind::Viewer), platinum("p2", SubjectKind::Viewer)];
    match verdict(&two_viewers) {
        Some((_, ValidationError::MultiplePlatinum)) => {}
        _ => panic!("同じ対象のプラチナ 2 件は MultiplePlatinum"),
    }
    let one_each = [platinum("p1", SubjectKind::Viewer), platinum("p2", SubjectKind::Streamer)];
    assert!(validate_all(&one_each).is_ok(), "対象が違えば 1 つずつ許す");
}

#[test]
fn validate_rejects_duplicate_id_and_empty_name() {
    let dup = [viewer_ok("same"), viewer_ok("same")];
    match verdict(&dup) {
        Some((id, ValidationError::DuplicateId)) => assert_eq!(id, "same"),
        _ => panic!("id の重複は DuplicateId"),
    }
    let mut unnamed = viewer_ok("n");
    unnamed.name = String::new();
    assert_err(&[unnamed], "n", |e| matches!(e, ValidationError::EmptyName), "空の名前");
}

#[test]
fn duplicate_drops_builtin_id_and_edited() {
    let mut src = presets().into_iter().find(|a| a.builtin_id.is_some() && a.condition.is_some()).expect("プリセット");
    src.edited = true;
    let copy = duplicate(&src, "my-copy");
    assert_eq!(copy.id, "my-copy");
    assert!(copy.builtin_id.is_none());
    assert!(!copy.edited);
    assert_eq!(to_json(&copy.condition), to_json(&src.condition), "条件はそのまま");
    assert_eq!(to_json(&copy.tier), to_json(&src.tier));
}

#[test]
fn reset_to_preset_restores_condition_keeps_id_and_enabled() {
    let original = presets().into_iter().find(|a| a.builtin_id.is_some() && a.condition.is_some()).expect("プリセット");
    let mut edited = presets().into_iter().find(|a| a.builtin_id == original.builtin_id).unwrap();
    edited.id = "my-id".to_string();
    edited.enabled = false;
    edited.edited = true;
    edited.condition.as_mut().unwrap().threshold = 987_654.0;
    let reset = reset_to_preset(&edited).expect("builtin_id があれば戻せる");
    assert_eq!(reset.id, "my-id");
    assert!(!reset.enabled);
    assert_eq!(reset.builtin_id, original.builtin_id);
    assert_eq!(to_json(&reset.condition), to_json(&original.condition));
}

#[test]
fn reset_to_preset_returns_none_without_builtin_id() {
    assert!(reset_to_preset(&viewer_ok("custom")).is_none());
}

#[test]
fn achievement_json_is_camel_case_roundtrip() {
    let mut a = make(
        "x1",
        SubjectKind::Streamer,
        Tier::Silver,
        Some(cond(Window::PerStream, Metric::UniqueCommenters, 7.0, CurrencyMode::None)),
    );
    a.builtin_id = Some("builtin-x".to_string());
    a.condition.as_mut().unwrap().platforms = vec![Platform::Youtube];
    let v = to_json(&a);
    assert_eq!(v["id"], "x1");
    assert_eq!(v["tier"], "silver");
    assert_eq!(v["subject"], "streamer");
    assert_eq!(v["builtinId"], "builtin-x");
    assert_eq!(v["condition"]["window"], "perStream");
    assert_eq!(v["condition"]["metric"], "uniqueCommenters");
    assert_eq!(v["condition"]["currency"], json!({"mode": "none"}));
    assert_eq!(v["condition"]["platforms"], json!(["youtube"]));
    assert_eq!(v["announce"]["tts"], false);
    let back: Achievement = serde_json::from_value(v.clone()).expect("読み戻せる");
    assert_eq!(to_json(&back), v);

    let mut b = viewer_amount("b", 5.0, CurrencyMode::Only("USD".to_string()));
    b.condition.as_mut().unwrap().metric = Metric::SuperChatCount;
    b.condition.as_mut().unwrap().currency = CurrencyMode::Only("USD".to_string());
    assert_eq!(to_json(&b)["condition"]["currency"], json!({"mode": "only", "code": "USD"}));
    assert_eq!(to_json(&b)["condition"]["metric"], "superChatCount");
}
