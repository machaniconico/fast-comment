//! 読み上げテンプレートの選択と展開(純関数)。

use crate::config::TtsTemplates;
use crate::model::MessageKind;

/// 名前と本文は呼び出し元で整形済み。金額が無い場合の `paid_text` は空文字。
pub struct TemplateVars<'a> {
    pub name: &'a str,
    pub text: &'a str,
    pub paid_text: &'a str,
}

/// 未指定の種別だけ共通設定へ戻す。明示的な空文字はそのまま採用する。
pub fn select_template(t: &TtsTemplates, kind: MessageKind) -> &str {
    let specific = match kind {
        MessageKind::Normal => &t.normal,
        MessageKind::SuperChat => &t.super_chat,
        MessageKind::Membership => &t.membership,
        MessageKind::Bits => &t.bits,
        MessageKind::Gift => &t.gift,
        MessageKind::System => &t.system,
    };
    specific.as_deref().unwrap_or(&t.all)
}

/// テンプレートだけを一度走査し、置換値の中のプレースホルダは展開しない。
pub fn render_template(template: &str, vars: &TemplateVars<'_>) -> String {
    let mut result = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        result.push_str(&rest[..start]);
        rest = &rest[start..];
        let Some(end) = rest.find('}') else {
            break;
        };
        let placeholder = &rest[..=end];
        result.push_str(match placeholder {
            "{name}" => vars.name,
            "{text}" => vars.text,
            "{paidText}" => vars.paid_text,
            _ => placeholder,
        });
        rest = &rest[end + 1..];
    }
    result.push_str(rest);
    result.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_template_matches_legacy_name_space_body() {
        let templates = TtsTemplates::default();
        for name in ["fast", "ファストさん", ""] {
            for text in ["ファスト URL省略", "", "こんにちは"] {
                let vars = TemplateVars {
                    name,
                    text,
                    paid_text: "",
                };
                assert_eq!(
                    render_template(select_template(&templates, MessageKind::Normal), &vars),
                    format!("{name} {text}").trim()
                );
            }
        }
    }

    #[test]
    fn kind_template_overrides_all_and_empty_kind_renders_empty() {
        let mut templates = TtsTemplates::default();
        let vars = TemplateVars {
            name: "名前",
            text: "本文",
            paid_text: "100円",
        };
        let kinds = [
            MessageKind::Normal,
            MessageKind::SuperChat,
            MessageKind::Membership,
            MessageKind::Bits,
            MessageKind::Gift,
            MessageKind::System,
        ];
        for kind in kinds {
            assert_eq!(select_template(&templates, kind), templates.all);
        }
        templates.normal = Some("通常 {text}".into());
        templates.super_chat = Some("支援 {paidText}".into());
        templates.membership = Some("参加 {name}".into());
        templates.bits = Some("ビッツ".into());
        templates.gift = Some("ギフト".into());
        templates.system = Some(String::new());
        for (kind, expected) in kinds.into_iter().zip([
            "通常 本文",
            "支援 100円",
            "参加 名前",
            "ビッツ",
            "ギフト",
            "",
        ]) {
            assert_eq!(
                render_template(select_template(&templates, kind), &vars),
                expected
            );
        }
    }

    #[test]
    fn unknown_placeholder_is_kept_literally() {
        let vars = TemplateVars {
            name: "名前",
            text: "本文",
            paid_text: "",
        };
        assert_eq!(
            render_template("{unknown} {名前} {} {Text} {text} 終端{", &vars),
            "{unknown} {名前} {} {Text} 本文 終端{"
        );
    }

    #[test]
    fn substituted_values_are_not_reexpanded() {
        let vars = TemplateVars {
            name: "{text}",
            text: "{paidText}",
            paid_text: "{name}",
        };
        assert_eq!(
            render_template("{name}{text}{paidText}{name}", &vars),
            "{text}{paidText}{name}{text}"
        );
    }

    #[test]
    fn paid_text_empty_when_no_amount() {
        let vars = TemplateVars {
            name: "名前",
            text: "本文",
            paid_text: "",
        };
        assert_eq!(render_template("{paidText}", &vars), "");
        assert_eq!(render_template("{paidText} {text}", &vars), "本文");
        let paid = TemplateVars {
            paid_text: "￥1,000",
            ..vars
        };
        assert_eq!(render_template("{paidText}", &paid), "￥1,000");
    }

    #[test]
    fn result_is_trimmed() {
        let vars = TemplateVars {
            name: "",
            text: "  本文  続き  ",
            paid_text: "",
        };
        assert_eq!(
            render_template("\n\t{name} {text} {paidText}　", &vars),
            "本文  続き"
        );
        assert_eq!(render_template(" \n\t　", &vars), "");
        assert_eq!(render_template("", &vars), "");
    }
}
