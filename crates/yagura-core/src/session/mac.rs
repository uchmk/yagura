//! macOS: the logins from `who`, the power mode from `pmset -g`.

#[cfg(target_os = "macos")]
use std::process::Command;

#[cfg(target_os = "macos")]
use super::Session;
use super::User;

#[cfg(target_os = "macos")]
pub fn read() -> Session {
    let run =
        |cmd: &str, args: &[&str]| Command::new(cmd).args(args).output().ok().map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
    Session {
        users: run("who", &[]).map(|s| parse_who(&s)).unwrap_or_default(),
        power: run("pmset", &["-g"]).and_then(|s| parse_pmset(&s)).unwrap_or_default(),
    }
}

/// `ann  console  Oct 10 09:12` and `bob  ttys001  Oct 10 10:01 (10.0.0.2)`.
pub fn parse_who(text: &str) -> Vec<User> {
    text.lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            let (name, line) = (w.next()?, w.next()?);
            let host = l.rfind('(').zip(l.rfind(')')).filter(|(a, b)| a < b).map(|(a, b)| l[a + 1..b].to_string()).unwrap_or_default();
            Some(User { name: name.to_string(), line: line.to_string(), host, since: 0 })
        })
        .collect()
}

/// `powermode` (0 automatic, 1 low, 2 high) on the newer Macs, else
/// `lowpowermode`.
pub fn parse_pmset(text: &str) -> Option<String> {
    let value = |key: &str| {
        text.lines().find_map(|l| {
            let mut w = l.split_whitespace();
            (w.next()? == key).then(|| w.next().map(str::to_string)).flatten()
        })
    };
    match (value("powermode").as_deref(), value("lowpowermode").as_deref()) {
        (Some("0"), _) => Some("Automatic".into()),
        (Some("1"), _) | (None, Some("1")) => Some("Low power".into()),
        (Some("2"), _) => Some("High power".into()),
        (None, Some("0")) => Some("Normal".into()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn who_lists_each_login() {
        let u = parse_who("ann      console  Oct 10 09:12\nbob      ttys001  Oct 10 10:01 (10.0.0.2)\n\n");
        assert_eq!(u.len(), 2);
        assert_eq!((u[0].name.as_str(), u[0].line.as_str(), u[0].host.as_str()), ("ann", "console", ""));
        assert_eq!(u[1].host, "10.0.0.2");
    }

    #[test]
    fn pmset_names_the_mode() {
        assert_eq!(parse_pmset(" sleep 1\n powermode            2\n"), Some("High power".into()));
        assert_eq!(parse_pmset(" lowpowermode 1\n"), Some("Low power".into()));
        assert_eq!(parse_pmset(" lowpowermode 0\n"), Some("Normal".into()));
        assert_eq!(parse_pmset(" sleep 1\n"), None);
    }
}
