//! Webhook notifications: a new alert is POSTed as JSON to the URL in `webhook` (tui.toml) or `YAGURA_WEBHOOK`.
//! The body carries the text under `text` (Slack, Mattermost, Teams) and `content` (Discord). `curl` does the sending
//! on a thread of its own, so a slow or dead endpoint never stops the screen.

use std::process::{Command, Stdio};

/// `s` as the inside of a JSON string.
fn escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o
}

/// The JSON body for the new alerts of `host`.
pub fn payload(host: &str, alerts: &[&str]) -> String {
    let text = escape(&format!("yagura {host}: {}", alerts.join("; ")));
    format!("{{\"text\":\"{text}\",\"content\":\"{text}\"}}")
}

/// Only an `http://` or `https://` address without blanks or control characters is sent to.
pub fn valid_url(url: &str) -> bool {
    (url.starts_with("http://") || url.starts_with("https://"))
        && url.len() > 8
        && !url.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// The `curl` arguments for one POST.
pub fn curl_args(url: &str, body: &str) -> Vec<String> {
    ["-sS", "-m", "10", "-X", "POST", "-H", "Content-Type: application/json", "-d", body, "--", url].iter().map(|s| s.to_string()).collect()
}

/// POST the new alerts on a thread of their own. A missing `curl` or a failed request is dropped.
pub fn send(url: &str, host: &str, alerts: &[&str]) {
    if !valid_url(url) || alerts.is_empty() {
        return;
    }
    let args = curl_args(url, &payload(host, alerts));
    let _ = std::thread::Builder::new().name("yagura-webhook".into()).spawn(move || {
        let _ = Command::new("curl").args(&args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_body_carries_the_alerts_as_escaped_json() {
        assert_eq!(payload("pve", &["a \"b\"", "c\\d"]), r#"{"text":"yagura pve: a \"b\"; c\\d","content":"yagura pve: a \"b\"; c\\d"}"#);
        assert_eq!(escape("x\ny\u{1}"), "x\\ny\\u0001");
    }

    #[test]
    fn only_plain_http_addresses_are_sent_to() {
        assert!(valid_url("https://hooks.example.com/x?a=1"));
        assert!(valid_url("http://10.0.0.5:8080/h"));
        assert!(!valid_url("ftp://x/y"));
        assert!(!valid_url("https://"));
        assert!(!valid_url("https://a b"));
        assert!(!valid_url(""));
        let a = curl_args("https://x/h", "{}");
        assert_eq!(&a[a.len() - 2..], ["--", "https://x/h"]);
    }
}
