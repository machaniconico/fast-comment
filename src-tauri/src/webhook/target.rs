pub use super::error::UrlError;
use crate::integrations::WebhookFormat;
use url::{Host, Url};
pub fn validate(format: WebhookFormat, raw: &str) -> Result<(), UrlError> {
    if raw.len() > 900
        || raw.chars().any(|c| c.is_whitespace() || c.is_control())
        || raw.contains('\\')
    {
        return Err(UrlError);
    }
    let authority = raw
        .split_once("://")
        .map(|(_, s)| s.split(['/', '?', '#']).next().unwrap_or_default())
        .ok_or(UrlError)?;
    if authority.is_empty() || authority.contains('@') {
        return Err(UrlError);
    }
    let u = Url::parse(raw).map_err(|_| UrlError)?;
    if u.host().is_none() || u.fragment().is_some() || !matches!(u.scheme(), "http" | "https") {
        return Err(UrlError);
    }
    match format {
        WebhookFormat::Discord => {
            if u.scheme() != "https"
                || authority.contains(':')
                || !matches!(
                    u.host_str(),
                    Some(
                        "discord.com" | "discordapp.com" | "canary.discord.com" | "ptb.discord.com"
                    )
                )
            {
                return Err(UrlError);
            }
            let re = regex::Regex::new(r"^/api/(v[0-9]+/)?webhooks/[^/]+/[^/]+/?$").unwrap();
            if !re.is_match(u.path()) {
                return Err(UrlError);
            }
        }
        WebhookFormat::Generic if u.scheme() == "http" => {
            let local = match u.host().unwrap() {
                Host::Domain(d) => d == "localhost",
                Host::Ipv4(ip) => ip.is_loopback() || ip.is_private() || ip.is_link_local(),
                Host::Ipv6(ip) => {
                    ip.is_loopback()
                        || (ip.segments()[0] & 0xfe00 == 0xfc00)
                        || (ip.segments()[0] & 0xffc0 == 0xfe80)
                }
            };
            if !local {
                return Err(UrlError);
            }
        }
        _ => {}
    }
    Ok(())
}
pub fn redact(raw: &str) -> String {
    match Url::parse(raw) {
        Ok(u) if u.host_str().is_some() && matches!(u.scheme(), "http" | "https") => {
            let tail: String = raw
                .chars()
                .rev()
                .take(4)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            format!("{}://{}/…{}", u.scheme(), u.host_str().unwrap(), tail)
        }
        _ => "URL 未設定".into(),
    }
}
