//! Pure template rendering. The caller injects local time; no application or I/O access.

use std::sync::OnceLock;

use chrono::{DateTime, Local};
use regex::Regex;
use serde::Serialize;

use super::error::{XPostError, XPostErrorCode};
use crate::model::{LiveStream, Platform};

const MAX_WEIGHT: usize = 280;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Rendered {
    pub text: String,
    pub weighted_len: usize,
    pub warnings: Vec<String>,
}

/// Sanitize only source titles. Template literals are intentionally preserved.
pub fn sanitize_title(title: &str) -> String {
    static FORMAT_CHARS: OnceLock<Regex> = OnceLock::new();
    // Only invisible/bidi controls. ZWJ/ZWNJ and emoji tag characters (also Cf)
    // are kept so emoji sequences such as 👩‍💻 stay intact.
    let format_chars = FORMAT_CHARS.get_or_init(|| {
        Regex::new(r"[\u{00AD}\u{061C}\u{180E}\u{200B}\u{200E}\u{200F}\u{202A}-\u{202E}\u{2060}-\u{2064}\u{2066}-\u{2069}\u{FEFF}]")
            .expect("format characters")
    });
    // Strip source format characters before adding our own mention separator.
    let title = format_chars.replace_all(title, "");
    let mut chars = title.chars().peekable();
    let mut result = String::new();
    while let Some(ch) = chars.next() {
        match ch {
            '@' | '＠' => result.push_str("＠\u{200b}"),
            '$' if chars.peek().is_some_and(char::is_ascii_alphabetic) => result.push('＄'),
            ch if ch.is_control() || (ch.is_whitespace() && ch != '\u{3000}') => result.push(' '),
            ch => result.push(ch),
        }
    }
    result
        .split(|ch: char| ch.is_whitespace() && ch != '\u{3000}')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn char_weight(ch: char) -> usize {
    match ch as u32 {
        0..=0x10ff | 0x2000..=0x200d | 0x2010..=0x201f | 0x2032..=0x2037 => 1,
        _ => 2,
    }
}

/// Conservative twitter-text approximation, including bare domain-like words.
pub fn weighted_len(text: &str) -> usize {
    static LINKS: OnceLock<Regex> = OnceLock::new();
    let links = LINKS.get_or_init(|| Regex::new(
        r"(?i)https?://[A-Za-z0-9\-._~:/?#\[\]@!$&'()*+,;=%]+|(?:[\p{L}\p{N}](?:[\p{L}\p{N}-]*[\p{L}\p{N}])?\.)+(?:[A-Za-z]{2,}|\p{L}{2,})(?:/[A-Za-z0-9\-._~:/?#\[\]@!$&'()*+,;=%]*)?"
    ).expect("static link pattern"));
    let mut total = 0;
    let mut end = 0;
    for link in links.find_iter(text) {
        total += text[end..link.start()]
            .chars()
            .map(char_weight)
            .sum::<usize>();
        // twitter-text never ends a URL with punctuation such as `.,;:!?'`:
        // those trailing characters are ordinary text (counted below).
        let raw = link
            .as_str()
            .trim_end_matches(|ch: char| !(ch.is_alphanumeric() || "=_#/-+".contains(ch)));
        let link_end = link.start() + raw.len();
        total += if raw.to_ascii_lowercase().starts_with("http://")
            || raw.to_ascii_lowercase().starts_with("https://")
        {
            if text[..link.start()]
                .chars()
                .next_back()
                .is_some_and(|ch| ch.is_ascii_alphanumeric())
            {
                raw.chars().map(char_weight).sum()
            } else {
                23
            }
        } else {
            23.max(raw.chars().map(char_weight).sum())
        };
        end = link_end;
    }
    total + text[end..].chars().map(char_weight).sum::<usize>()
}

enum Part<'a> {
    Literal(&'a str),
    Title,
    Value(String),
}

fn compose(parts: &[Part<'_>], title: &str) -> String {
    let mut lines = vec![(String::new(), false)];
    for part in parts {
        let (value, substituted) = match part {
            Part::Literal(value) => (*value, false),
            Part::Title => (title, true),
            Part::Value(value) => (value.as_str(), true),
        };
        for (index, fragment) in value.split('\n').enumerate() {
            if index > 0 {
                lines.push((String::new(), false));
            }
            let (line, changed) = lines.last_mut().expect("at least one line");
            line.push_str(fragment);
            *changed |= substituted;
        }
    }
    lines
        .into_iter()
        .filter(|(line, changed)| !changed || !line.trim().is_empty())
        .map(|(line, _)| line)
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn render(
    template: &str,
    streams: &[LiveStream],
    now_local: DateTime<Local>,
) -> Result<Rendered, XPostError> {
    let mut streams: Vec<_> = streams.iter().collect();
    streams.sort_by_key(|stream| match stream.platform {
        Platform::Youtube => 0,
        Platform::Twitch => 1,
        Platform::Niconico => 2,
        Platform::X => 3,
    });
    let title = sanitize_title(
        streams
            .first()
            .and_then(|stream| stream.title.as_deref())
            .unwrap_or(""),
    );
    let urls = streams
        .iter()
        .map(|stream| stream.url.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let mut platforms = Vec::new();
    for stream in &streams {
        let name = match stream.platform {
            Platform::Youtube => "YouTube",
            Platform::Twitch => "Twitch",
            Platform::Niconico => "ニコ生",
            Platform::X => "X",
        };
        if !platforms.contains(&name) {
            platforms.push(name);
        }
    }
    let mut parts = Vec::new();
    let mut warnings = Vec::new();
    let mut rest = template;
    let mut has_url = false;
    // Parse the template once. Braces in source titles and URLs are never expanded.
    while let Some(start) = rest.find('{') {
        parts.push(Part::Literal(&rest[..start]));
        rest = &rest[start..];
        let Some(end) = rest.find('}') else { break };
        if let Some(nested) = rest[1..end].find('{') {
            let nested = nested + 1;
            parts.push(Part::Literal(&rest[..nested]));
            rest = &rest[nested..];
            continue;
        }
        let variable = &rest[..=end];
        parts.push(match variable {
            "{title}" => Part::Title,
            "{url}" => {
                has_url = true;
                Part::Value(urls.clone())
            }
            "{platform}" => Part::Value(platforms.join(" / ")),
            "{date}" => Part::Value(now_local.format("%Y/%m/%d").to_string()),
            "{time}" => Part::Value(now_local.format("%H:%M").to_string()),
            _ => {
                if !warnings
                    .iter()
                    .any(|warning: &String| warning.contains(variable))
                {
                    warnings.push(format!("未知の変数: {variable}"));
                }
                Part::Literal(variable)
            }
        });
        rest = &rest[end + 1..];
    }
    parts.push(Part::Literal(rest));
    if !has_url {
        return Err(XPostError::new(XPostErrorCode::MissingUrl));
    }
    let mut text = compose(&parts, &title);
    if weighted_len(&text) > MAX_WEIGHT {
        if weighted_len(&compose(&parts, "")) > MAX_WEIGHT {
            return Err(XPostError::new(XPostErrorCode::TooLong));
        }
        // Evaluate complete candidates: repeated placeholders and URL recognition
        // can make weights non-linear. Never truncate the rendered post itself.
        let mut shortened = title.clone();
        loop {
            if shortened.pop().is_none() {
                text = compose(&parts, "");
                break;
            }
            text = compose(&parts, &format!("{shortened}…"));
            if weighted_len(&text) <= MAX_WEIGHT {
                break;
            }
        }
    }
    Ok(Rendered {
        weighted_len: weighted_len(&text),
        text,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, TimeZone};

    fn now() -> DateTime<Local> {
        FixedOffset::east_opt(9 * 3600)
            .unwrap()
            .with_ymd_and_hms(2026, 11, 14, 12, 34, 0)
            .unwrap()
            .with_timezone(&Local)
    }

    fn stream(platform: Platform, title: Option<&str>, url: &str) -> LiveStream {
        LiveStream {
            platform,
            channel_identifier: "channel".into(),
            stream_id: "stream".into(),
            title: title.map(str::to_owned),
            url: url.into(),
            started_at_ms: None,
        }
    }

    #[test]
    fn variables_unknowns_and_empty_lines() {
        let time = now();
        let result = render(
            "{title}\n\n  \n{url}\n{platform}\n{date} {time}\n{unknown}",
            &[stream(Platform::Youtube, None, "https://youtu.be/abc")],
            time,
        )
        .unwrap();
        assert_eq!(
            result.text,
            format!(
                "\n  \nhttps://youtu.be/abc\nYouTube\n{}\n{{unknown}}",
                time.format("%Y/%m/%d %H:%M")
            )
        );
        assert_eq!(result.warnings, ["未知の変数: {unknown}"]);
        assert_eq!(result.weighted_len, weighted_len(&result.text));
    }

    #[test]
    fn url_placeholder_is_required_even_with_literal_url() {
        for template in ["{title}", "https://example.com", "{URL}", ""] {
            assert_eq!(
                render(template, &[], now()).unwrap_err().code,
                XPostErrorCode::MissingUrl
            );
        }
    }

    #[test]
    fn priority_combines_all_urls_and_platforms_without_mutating_input() {
        let streams = [
            stream(Platform::Twitch, Some("second"), "https://twitch.tv/test"),
            stream(Platform::Youtube, Some("first"), "https://youtu.be/abc"),
            stream(
                Platform::Niconico,
                Some("third"),
                "https://live.nicovideo.jp/watch/lv123",
            ),
        ];
        let result = render("{title}\n{url}\n{platform}", &streams[..2], now()).unwrap();
        assert_eq!(
            result.text,
            "first\nhttps://youtu.be/abc\nhttps://twitch.tv/test\nYouTube / Twitch"
        );
        assert_eq!(streams[0].platform, Platform::Twitch);
        assert!(render("{url}\n{platform}", &streams, now())
            .unwrap()
            .text
            .ends_with("YouTube / Twitch / ニコ生"));
    }

    #[test]
    fn sanitize_source_title_but_preserve_template_and_inserted_braces() {
        assert_eq!(
            sanitize_title(" @name\n\r\t$ABC  $abc $123 #tag\0end "),
            "＠\u{200b}name ＄ABC ＄abc $123 #tag end"
        );
        let result = render(
            "@literal $ABC\n{title}\n{url}",
            &[stream(
                Platform::Youtube,
                Some("@name $ABC {date}"),
                "https://youtu.be/abc",
            )],
            now(),
        )
        .unwrap();
        assert_eq!(
            result.text,
            "@literal $ABC\n＠\u{200b}name ＄ABC {date}\nhttps://youtu.be/abc"
        );
        assert!(result.warnings.is_empty());
    }

    #[test]
    fn weighted_unicode_and_link_rules() {
        assert_eq!(weighted_len("abc日本語😀"), 11);
        assert_eq!(
            weighted_len("\u{10ff}\u{1100}\u{2000}\u{200d}\u{2010}\u{201f}\u{2032}\u{2037}"),
            9
        );
        assert_eq!(
            weighted_len("https://example.com/very/long/path http://a.co"),
            47
        );
        assert_eq!(weighted_len("example.com"), 23);
        let domain = format!("{}.example.com", "a".repeat(40));
        assert_eq!(weighted_len(&domain), domain.len());
        assert_eq!(weighted_len("👩\u{200d}💻"), 5);
    }

    #[test]
    fn trailing_punctuation_and_following_text_are_not_part_of_links() {
        let url = "https://twitch.tv/mine";
        let dots = format!("{url}{}", ".".repeat(258));
        assert_eq!(weighted_len(&dots), 23 + 258);
        let streams = [stream(Platform::Twitch, None, url)];
        let template = format!("{{url}}{}", ".".repeat(258));
        assert_eq!(
            render(&template, &streams, now()).unwrap_err().code,
            XPostErrorCode::TooLong
        );
        assert_eq!(weighted_len(&format!("{url}!?")), 25);
        assert_eq!(weighted_len(&format!("{url}/a_b")), 23);
        // Bare domains stop at whitespace-free trailing text.
        assert_eq!(weighted_len("example.comです"), 23 + 4);
        assert_eq!(weighted_len("example.com/path."), 24);
    }

    #[test]
    fn exact_limit_and_title_truncation_preserve_url_and_literals() {
        let url = "https://youtube.com/watch?v=abcdefghijklmnopqrstuvwxyz";
        let mut streams = [stream(Platform::Youtube, Some(&"a".repeat(256)), url)];
        let exact = render("{title}\n{url}", &streams, now()).unwrap();
        assert_eq!(exact.weighted_len, 280);
        assert!(!exact.text.contains('…'));
        streams[0].title = Some("日".repeat(300));
        let result = render("【開始】{title}\n{url}", &streams, now()).unwrap();
        assert!(result.weighted_len <= 280);
        assert!(result.text.starts_with("【開始】"));
        assert!(result.text.ends_with(&format!("…\n{url}")));
        assert_eq!(result.text.matches(url).count(), 1);
        let repeated = render("{title}\n{title}\n{url}", &streams, now()).unwrap();
        let lines: Vec<_> = repeated.text.lines().collect();
        assert_eq!(lines[0], lines[1]);
        assert!(repeated.weighted_len <= 280);
    }

    #[test]
    fn urls_touching_text_are_counted_without_swallowing_suffixes() {
        let url = format!("https://example.com/{}", "a".repeat(80));
        let streams = [stream(Platform::Youtube, None, &url)];
        let japanese = render("{url}です", &streams, now()).unwrap();
        assert_eq!(japanese.text, format!("{url}です"));
        assert_eq!(japanese.weighted_len, 27);
        let prefixed = render("x{url}", &streams, now()).unwrap();
        assert_eq!(prefixed.weighted_len, 1 + url.len());
        assert_eq!(weighted_len(&format!("9{url}")), 1 + url.len());
        assert_eq!(weighted_len(&format!("日本{url}")), 4 + 23);
        assert_eq!(weighted_len(&format!("{url}。次")), 27);
        let overflowing = format!("{}{{url}}です", "日".repeat(127));
        assert_eq!(
            render(&overflowing, &streams, now()).unwrap_err().code,
            XPostErrorCode::TooLong
        );
    }

    #[test]
    fn title_mentions_and_unicode_controls_are_sanitized() {
        assert_eq!(
            sanitize_title("@alice ＠bob @\u{200b}carol ＠\u{202e}dan"),
            "＠\u{200b}alice ＠\u{200b}bob ＠\u{200b}carol ＠\u{200b}dan"
        );
        assert_eq!(sanitize_title("　日本　語　"), "　日本　語　");
        assert_eq!(sanitize_title("a\u{202e}b\u{200b}c\n\t\0d"), "abc d");
        assert_eq!(sanitize_title("$\u{200b}ABC $123"), "＄ABC $123");
        // Emoji ZWJ sequences survive; bidi controls do not.
        assert_eq!(sanitize_title("👩\u{200d}💻作業配信"), "👩\u{200d}💻作業配信");
        assert_eq!(sanitize_title("a\u{2066}b\u{feff}c\u{200e}d"), "abcd");
    }

    #[test]
    fn unclosed_braces_do_not_hide_valid_placeholders() {
        let streams = [stream(
            Platform::Youtube,
            Some("title"),
            "https://youtu.be/abc",
        )];
        for (template, expected) in [
            ("{ {url}", "{ https://youtu.be/abc"),
            ("{title} {\n{url}", "title {\nhttps://youtu.be/abc"),
            ("{{{url}", "{{https://youtu.be/abc"),
            ("{ {unknown} {url}", "{ {unknown} https://youtu.be/abc"),
        ] {
            let result = render(template, &streams, now()).unwrap();
            assert_eq!(result.text, expected);
            assert_eq!(
                result.warnings.len(),
                usize::from(template.contains("{unknown}"))
            );
        }
    }

    #[test]
    fn only_lines_emptied_by_substitution_are_removed() {
        let streams = [stream(Platform::Youtube, None, "https://youtu.be/abc")];
        let result = render("\n  \n{title}\n \t{title}\n{url}\n\n", &streams, now()).unwrap();
        assert_eq!(result.text, "\n  \nhttps://youtu.be/abc\n\n");
    }

    #[test]
    fn empty_title_is_used_when_even_ellipsis_does_not_fit() {
        let streams = [stream(
            Platform::Youtube,
            Some("long title"),
            "https://youtu.be/abc",
        )];
        for fixed_len in [255, 256] {
            let fixed = "a".repeat(fixed_len);
            let result = render(&format!("{fixed}{{title}}\n{{url}}"), &streams, now()).unwrap();
            assert_eq!(result.text, format!("{fixed}\nhttps://youtu.be/abc"));
            assert_eq!(result.weighted_len, fixed_len + 24);
        }
    }

    #[test]
    fn fixed_text_cannot_be_truncated() {
        let streams = [stream(
            Platform::Youtube,
            Some("title"),
            "https://youtu.be/abc",
        )];
        assert_eq!(
            render(&format!("{}\n{{url}}", "日".repeat(140)), &streams, now())
                .unwrap_err()
                .code,
            XPostErrorCode::TooLong
        );
        assert_eq!(render("{url}", &[], now()).unwrap().text, "");
        let result = render("before {url} after {unfinished", &streams, now()).unwrap();
        assert!(result.text.ends_with("after {unfinished"));
    }
}
