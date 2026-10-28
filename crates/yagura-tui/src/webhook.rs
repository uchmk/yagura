//! Webhook notifications: a new alert is POSTed as JSON to the URL in `webhook` (tui.toml) or `YAGURA_WEBHOOK`.
//! The body carries the text under `text` (Slack, Mattermost, Teams) and `content` (Discord). `curl` does the sending
//! on a thread of its own, so a slow or dead endpoint never stops the screen.

use std::process::{Command, Stdio};
use std::sync::Mutex;

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

/// Whether the alert with this key (`storage nas`, `vzdump UPID…`, `temp`) is of a kind to send; no kinds listed means all.
pub fn wanted(kinds: &[String], key: &str) -> bool {
    let kind = key.split(' ').next().unwrap_or("");
    kinds.is_empty() || kinds.iter().any(|k| k == kind)
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

/// The `curl` arguments for one POST. `ntfy` sends the text as it is with a title and a priority (ntfy.sh and a self-hosted ntfy);
/// anything else sends the JSON body.
pub fn curl_args(url: &str, host: &str, alerts: &[&str], format: &str) -> Vec<String> {
    let mut a: Vec<String> = ["-sS", "-m", "10", "-X", "POST"].iter().map(|s| s.to_string()).collect();
    if format == "ntfy" {
        // A header value is one line without control characters.
        let title: String = format!("yagura {host}").chars().filter(|c| !c.is_control()).collect();
        for h in [format!("Title: {title}"), "Priority: high".to_string(), "Tags: warning".to_string()] {
            a.extend(["-H".to_string(), h]);
        }
        a.extend([
            "-d".to_string(),
            alerts.join(
                "
",
            ),
        ]);
    } else {
        a.extend(["-H".to_string(), "Content-Type: application/json".to_string(), "-d".to_string(), payload(host, alerts)]);
    }
    a.extend(["--".to_string(), url.to_string()]);
    a
}

/// Seconds to wait before each retry of a failed POST.
const RETRY_AFTER: [u64; 2] = [5, 30];

/// What the last send that gave up said; `take_failure` hands it to the screen once.
static FAILURE: Mutex<Option<String>> = Mutex::new(None);

/// The one line that says why a send failed: curl's own message when it printed one, else its exit code.
pub fn failure_text(code: Option<i32>, stderr: &str) -> String {
    let line = stderr.lines().map(str::trim).find(|l| !l.is_empty());
    match (line, code) {
        (Some(l), _) => l.trim_start_matches("curl: ").chars().take(120).collect(),
        (None, Some(c)) => format!("curl exited with {c}"),
        (None, None) => "curl was stopped".to_string(),
    }
}

/// The failure of a send that used up its retries, once; `None` when there is nothing new.
pub fn take_failure() -> Option<String> {
    FAILURE.lock().ok().and_then(|mut f| f.take())
}

/// POST the new alerts on a thread of their own. A failed request is tried again after 5 s and 30 s; when the last try
/// fails too, `take_failure` says why. A missing `curl` counts as a failure.
pub fn send(url: &str, host: &str, alerts: &[&str], format: &str) {
    if !valid_url(url) || alerts.is_empty() {
        return;
    }
    let args = curl_args(url, host, alerts, format);
    let _ = std::thread::Builder::new().name("yagura-webhook".into()).spawn(move || {
        let mut why = String::new();
        for wait in std::iter::once(0).chain(RETRY_AFTER) {
            std::thread::sleep(std::time::Duration::from_secs(wait));
            match Command::new("curl").args(&args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).output() {
                Ok(o) if o.status.success() => return,
                Ok(o) => why = failure_text(o.status.code(), &String::from_utf8_lossy(&o.stderr)),
                Err(e) => why = format!("cannot run curl: {e}"),
            }
        }
        if let Ok(mut f) = FAILURE.lock() {
            *f = Some(why);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_send_says_why_in_one_line() {
        assert_eq!(
            failure_text(
                Some(6),
                "curl: (6) Could not resolve host: x
"
            ),
            "(6) Could not resolve host: x"
        );
        assert_eq!(
            failure_text(
                Some(22),
                "  
"
            ),
            "curl exited with 22"
        );
        assert_eq!(failure_text(None, ""), "curl was stopped");
        assert_eq!(failure_text(Some(1), &"x".repeat(300)).len(), 120);
    }

    #[test]
    fn the_body_carries_the_alerts_as_escaped_json() {
        assert_eq!(payload("pve", &["a \"b\"", "c\\d"]), r#"{"text":"yagura pve: a \"b\"; c\\d","content":"yagura pve: a \"b\"; c\\d"}"#);
        assert_eq!(escape("x\ny\u{1}"), "x\\ny\\u0001");
    }

    #[test]
    fn alerts_are_filtered_by_kind() {
        let k = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(wanted(&[], "storage nas"));
        assert!(wanted(&k(&["storage", "temp"]), "storage nas"));
        assert!(wanted(&k(&["temp"]), "temp"));
        assert!(!wanted(&k(&["storage"]), "vzdump UPID:pve:1"));
        assert!(!wanted(&k(&["stale"]), "failed 101 5"));
    }

    #[test]
    fn only_plain_http_addresses_are_sent_to() {
        assert!(valid_url("https://hooks.example.com/x?a=1"));
        assert!(valid_url("http://10.0.0.5:8080/h"));
        assert!(!valid_url("ftp://x/y"));
        assert!(!valid_url("https://"));
        assert!(!valid_url("https://a b"));
        assert!(!valid_url(""));
        let a = curl_args("https://x/h", "pve", &["a"], "json");
        assert_eq!(&a[a.len() - 2..], ["--", "https://x/h"]);
        assert!(a.contains(&"Content-Type: application/json".to_string()));
    }

    #[test]
    fn ntfy_gets_plain_text_with_a_title() {
        let a = curl_args(
            "https://ntfy.sh/t",
            "pve
x",
            &["a", "b"],
            "ntfy",
        );
        assert!(a.contains(&"Title: yagura pvex".to_string()) && a.contains(&"Priority: high".to_string()));
        let i = a.iter().position(|x| x == "-d").unwrap();
        assert_eq!(
            a[i + 1],
            "a
b"
        );
        assert!(!a.iter().any(|x| x.contains("json")));
    }
}
