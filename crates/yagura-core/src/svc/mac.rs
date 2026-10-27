//! launchd's jobs: `launchctl list` for the loaded ones (a PID or `-`, the
//! last exit status and the label, one job a line), and the plists in
//! launchd's folders for the ones not loaded (disabled, mostly) and for
//! every job's command, account and file. `launchctl print-disabled` says
//! which are switched off. As a user it is that user's agents; the system's
//! daemons need root: the same domains `svcctl` sends to.

use std::collections::HashMap;

use super::{Service, Start, State};

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn geteuid() -> u32;
}

#[cfg(target_os = "macos")]
pub fn uid() -> u32 {
    // SAFETY: geteuid takes nothing and cannot fail.
    unsafe { geteuid() }
}

#[cfg(target_os = "macos")]
pub fn root() -> bool {
    uid() == 0
}

/// The plists already read, kept while the file's time is the same: the
/// system's folders hold some 400 that never change.
#[cfg(target_os = "macos")]
#[derive(Default)]
pub struct Cache {
    jobs: HashMap<std::path::PathBuf, (std::time::SystemTime, Option<Job>)>,
}

#[cfg(target_os = "macos")]
pub fn read(cache: &mut Cache) -> super::Services {
    use std::process::{Command, Stdio};

    use super::{Note, Services};

    let root = root();
    let run = |args: &[&str]| {
        Command::new("launchctl")
            .args(args)
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    };
    let list = run(&["list"]).map(|s| parse_list(&s)).unwrap_or_default();
    let domain = if root { "system".to_string() } else { format!("gui/{}", uid()) };
    let disabled = run(&["print-disabled", &domain]).map(|s| parse_disabled(&s)).unwrap_or_default();
    let jobs = cache.jobs(&folders(root));
    Services { list: merge(list, &jobs, &disabled, root), source: "launchctl", note: (!root).then_some(Note::MacUser) }
}

/// The folders of the domain `launchctl list` shows: the daemons as root,
/// the agents (the user's own first) otherwise.
#[cfg(target_os = "macos")]
fn folders(root: bool) -> Vec<std::path::PathBuf> {
    use std::path::PathBuf;

    if root {
        return vec!["/Library/LaunchDaemons".into(), "/System/Library/LaunchDaemons".into()];
    }
    let mut v: Vec<PathBuf> = ito_common::home_dir().map(|h| h.join("Library/LaunchAgents")).into_iter().collect();
    v.extend(["/Library/LaunchAgents", "/System/Library/LaunchAgents"].map(PathBuf::from));
    v
}

#[cfg(target_os = "macos")]
impl Cache {
    /// Every job in `folders`, with its file; a plist that cannot be read is
    /// left out.
    fn jobs(&mut self, folders: &[std::path::PathBuf]) -> Vec<(String, Job)> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for dir in folders {
            let Ok(entries) = std::fs::read_dir(dir) else { continue };
            let mut paths: Vec<_> = entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "plist")).collect();
            paths.sort();
            for p in paths {
                let Ok(time) = std::fs::metadata(&p).and_then(|m| m.modified()) else { continue };
                seen.insert(p.clone());
                let job = match self.jobs.get(&p) {
                    Some((t, job)) if *t == time => job.clone(),
                    _ => {
                        let job = std::fs::read(&p).ok().and_then(|b| parse_plist(&b));
                        self.jobs.insert(p.clone(), (time, job.clone()));
                        job
                    }
                };
                if let Some(job) = job {
                    out.push((p.display().to_string(), job));
                }
            }
        }
        // A plist that went away goes from the cache too.
        self.jobs.retain(|p, _| seen.contains(p));
        out
    }
}

/// The rows under the `PID Status Label` header. A job with a PID runs; one
/// without is loaded and waiting, failed when its last exit was not 0 (a
/// negative status is the signal that ended it).
pub fn parse_list(s: &str) -> Vec<Service> {
    s.lines()
        .filter_map(|l| {
            let mut it = l.split('\t');
            let (pid, status, label) = (it.next()?.trim(), it.next()?.trim(), it.next()?.trim());
            if label.is_empty() || pid == "PID" {
                return None;
            }
            let pid = pid.parse::<u32>().ok();
            let exit = status.parse::<i64>().ok();
            let state = match (pid, exit) {
                (Some(_), _) => State::Running,
                (None, Some(0) | None) => State::Stopped,
                (None, Some(_)) => State::Failed,
            };
            Some(Service { name: label.to_string(), state, pid, exit, ..Service::default() })
        })
        .collect()
}

/// What a job's plist says about it.
#[derive(Clone, Debug, PartialEq)]
pub struct Job {
    pub label: String,
    /// The program and its arguments, as one line.
    pub cmd: Option<String>,
    pub user: Option<String>,
    /// When launchd starts it, in launchd's terms.
    pub start: Start,
    pub start_raw: &'static str,
    /// `Disabled` in the plist: the default `print-disabled` can override.
    pub disabled: bool,
}

/// A launchd plist, XML or binary. `None` when it is not one or has no label.
pub fn parse_plist(bytes: &[u8]) -> Option<Job> {
    let v = plist::Value::from_reader(std::io::Cursor::new(bytes)).ok()?;
    let d = v.as_dictionary()?;
    let label = d.get("Label")?.as_string()?.trim();
    if label.is_empty() {
        return None;
    }
    let flag = |k: &str| d.get(k).and_then(plist::Value::as_boolean).unwrap_or(false);
    let args: Vec<&str> = d
        .get("ProgramArguments")
        .and_then(plist::Value::as_array)
        .map(|a| a.iter().filter_map(plist::Value::as_string).collect())
        .unwrap_or_default();
    // With `Program`, the first argument is only the name the program sees.
    let line: Vec<&str> = match d.get("Program").and_then(plist::Value::as_string) {
        Some(p) => std::iter::once(p).chain(args.iter().skip(1).copied()).collect(),
        None => args,
    };
    let cmd = (!line.is_empty()).then(|| line.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" "));
    let keep = d.get("KeepAlive");
    let (start, start_raw) = if keep.and_then(plist::Value::as_boolean) == Some(true) {
        (Start::Auto, "keep alive")
    } else if flag("RunAtLoad") {
        (Start::Auto, "at load")
    } else if keep.and_then(plist::Value::as_dictionary).is_some() {
        (Start::Manual, "keep alive if")
    } else if d.contains_key("StartInterval") || d.contains_key("StartCalendarInterval") {
        (Start::Manual, "on a schedule")
    } else {
        (Start::Manual, "on demand")
    };
    let user = d.get("UserName").and_then(plist::Value::as_string).map(str::to_string);
    Some(Job { label: label.to_string(), cmd, user, start, start_raw, disabled: flag("Disabled") })
}

fn quote(a: &str) -> String {
    if a.is_empty() || a.contains([' ', '\t', '"']) { format!("\"{}\"", a.replace('"', "\\\"")) } else { a.to_string() }
}

/// `launchctl print-disabled`'s `"label" => disabled` lines (`=> true` on
/// older macOS): the jobs switched off or on over their plist's default.
pub fn parse_disabled(s: &str) -> HashMap<String, bool> {
    s.lines()
        .filter_map(|l| {
            let (label, v) = l.split_once("=>")?;
            let label = label.trim().trim_matches('"');
            let off = match v.trim() {
                "disabled" | "true" => true,
                "enabled" | "false" => false,
                _ => return None,
            };
            (!label.is_empty()).then(|| (label.to_string(), off))
        })
        .collect()
}

/// The loaded jobs with what their plists add, then the plists' jobs that
/// are not loaded, as stopped and `unloaded`. The first plist of a label wins (the user's
/// folder comes first). Daemons without `UserName` run as root.
pub fn merge(mut list: Vec<Service>, jobs: &[(String, Job)], disabled: &HashMap<String, bool>, daemons: bool) -> Vec<Service> {
    let mut at: HashMap<String, usize> = list.iter().enumerate().map(|(i, s)| (s.name.clone(), i)).collect();
    let mut done = std::collections::HashSet::new();
    for (file, job) in jobs {
        if !done.insert(job.label.as_str()) {
            continue;
        }
        let off = disabled.get(&job.label).copied().unwrap_or(job.disabled);
        let (start, start_raw) = if off { (Start::Disabled, "disabled") } else { (job.start, job.start_raw) };
        let user = job.user.clone().or_else(|| daemons.then(|| "root".to_string()));
        let i = *at.entry(job.label.clone()).or_insert_with(|| {
            let sub = "not loaded".to_string();
            list.push(Service { name: job.label.clone(), state: State::Stopped, sub, unloaded: true, ..Service::default() });
            list.len() - 1
        });
        let s = &mut list[i];
        s.start = start;
        s.start_raw = start_raw.to_string();
        s.cmd = job.cmd.clone();
        s.user = user;
        s.file = Some(file.clone());
    }
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launchctl_rows_become_jobs() {
        let v = parse_list(
            "PID\tStatus\tLabel\n\
             412\t0\tcom.apple.Finder\n\
             -\t0\tcom.apple.SafariHistoryServiceAgent\n\
             -\t-9\tcom.example.killed\n\
             -\t78\tcom.example.broken\n\
             not a row\n",
        );
        let got: Vec<(&str, State, Option<u32>, Option<i64>)> = v.iter().map(|s| (s.name.as_str(), s.state, s.pid, s.exit)).collect();
        assert_eq!(
            got,
            [
                ("com.apple.Finder", State::Running, Some(412), Some(0)),
                ("com.apple.SafariHistoryServiceAgent", State::Stopped, None, Some(0)),
                ("com.example.killed", State::Failed, None, Some(-9)),
                ("com.example.broken", State::Failed, None, Some(78)),
            ]
        );
    }

    const XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>com.example.sync</string>
    <key>ProgramArguments</key>
    <array><string>/usr/local/bin/sync</string><string>--dir</string><string>/Users/me/My Files</string></array>
    <key>RunAtLoad</key><true/>
    <key>UserName</key><string>_sync</string>
</dict>
</plist>"#;

    #[test]
    fn plists_say_the_command_the_account_and_when_it_starts() {
        let j = parse_plist(XML.as_bytes()).unwrap();
        assert_eq!(j.label, "com.example.sync");
        assert_eq!(j.cmd.as_deref(), Some("/usr/local/bin/sync --dir \"/Users/me/My Files\""));
        assert_eq!(j.user.as_deref(), Some("_sync"));
        assert_eq!((j.start, j.start_raw, j.disabled), (Start::Auto, "at load", false));

        // Most of the system's plists are binary.
        let mut d = plist::Dictionary::new();
        d.insert("Label".into(), "com.apple.thing".into());
        d.insert("Program".into(), "/usr/libexec/thing".into());
        d.insert("ProgramArguments".into(), plist::Value::Array(vec!["thing".into(), "-v".into()]));
        d.insert("KeepAlive".into(), plist::Value::Dictionary(plist::Dictionary::new()));
        d.insert("Disabled".into(), true.into());
        let mut bin = Vec::new();
        plist::Value::Dictionary(d.clone()).to_writer_binary(&mut bin).unwrap();
        let j = parse_plist(&bin).unwrap();
        assert_eq!(j.cmd.as_deref(), Some("/usr/libexec/thing -v"));
        assert_eq!((j.start, j.start_raw, j.disabled, j.user), (Start::Manual, "keep alive if", true, None));

        d.insert("KeepAlive".into(), true.into());
        d.remove("Disabled");
        let mut bin = Vec::new();
        plist::Value::Dictionary(d.clone()).to_writer_binary(&mut bin).unwrap();
        assert_eq!(parse_plist(&bin).unwrap().start_raw, "keep alive");
        d.remove("KeepAlive");
        d.insert("StartCalendarInterval".into(), plist::Value::Dictionary(plist::Dictionary::new()));
        let mut bin = Vec::new();
        plist::Value::Dictionary(d).to_writer_binary(&mut bin).unwrap();
        assert_eq!(parse_plist(&bin).unwrap().start_raw, "on a schedule");

        assert_eq!(parse_plist(b"not a plist"), None);
        assert_eq!(parse_plist(XML.replace("com.example.sync", "").as_bytes()), None);
    }

    #[test]
    fn print_disabled_lines_old_and_new() {
        let m = parse_disabled(
            "disabled services = {\n\
             \t\"com.openssh.sshd\" => disabled\n\
             \t\"com.apple.ftp-proxy\" => enabled\n\
             \t\"com.example.old\" => true\n\
             }\n\
             login item associations = {\n\
             }\n",
        );
        assert_eq!(m.len(), 3);
        assert_eq!(m.get("com.openssh.sshd"), Some(&true));
        assert_eq!(m.get("com.apple.ftp-proxy"), Some(&false));
        assert_eq!(m.get("com.example.old"), Some(&true));
    }

    fn job(label: &str, disabled: bool) -> Job {
        Job { label: label.into(), cmd: Some(format!("/bin/{label}")), user: None, start: Start::Auto, start_raw: "at load", disabled }
    }

    #[test]
    fn plists_fill_the_loaded_jobs_and_add_the_ones_not_loaded() {
        let list = parse_list("PID\tStatus\tLabel\n412\t0\ta\n-\t0\tanon\n");
        let jobs = [
            ("/Library/LaunchDaemons/a.plist".to_string(), job("a", false)),
            ("/Library/LaunchDaemons/off.plist".to_string(), job("off", true)),
            ("/Library/LaunchDaemons/on.plist".to_string(), job("on", true)),
            ("/System/Library/LaunchDaemons/a.plist".to_string(), job("a", true)),
        ];
        let disabled = HashMap::from([("on".to_string(), false)]);
        let v = merge(list, &jobs, &disabled, true);
        type Row<'a> = (&'a str, State, Start, &'a str, Option<&'a str>, Option<&'a str>);
        let got: Vec<Row> =
            v.iter().map(|s| (s.name.as_str(), s.state, s.start, s.start_raw.as_str(), s.user.as_deref(), s.file.as_deref())).collect();
        assert_eq!(
            got,
            [
                ("a", State::Running, Start::Auto, "at load", Some("root"), Some("/Library/LaunchDaemons/a.plist")),
                // A job with no plist (an app's, an XPC service) stays as listed.
                ("anon", State::Stopped, Start::Unknown, "", None, None),
                ("off", State::Stopped, Start::Disabled, "disabled", Some("root"), Some("/Library/LaunchDaemons/off.plist")),
                ("on", State::Stopped, Start::Auto, "at load", Some("root"), Some("/Library/LaunchDaemons/on.plist")),
            ]
        );
        assert_eq!(v[0].cmd.as_deref(), Some("/bin/a"));
        let unloaded: Vec<(&str, &str)> = v.iter().filter(|s| s.unloaded).map(|s| (s.name.as_str(), s.sub.as_str())).collect();
        assert_eq!(unloaded, [("off", "not loaded"), ("on", "not loaded")]);
        // An agent runs as whoever logged in: no account is made up.
        assert_eq!(merge(Vec::new(), &jobs[..1], &HashMap::new(), false)[0].user, None);
    }
}
