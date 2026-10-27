//! systemd's services through `systemctl`: `show` for the loaded units (one
//! call for every unit, as `Key=Value` blocks) and `list-unit-files` for the
//! ones not loaded, the disabled ones mostly. Without systemd as PID 1 (a
//! container, WSL without systemd) only the unit files can be listed.

use super::{Service, Start, State};

/// The properties `show` is asked for.
#[cfg(target_os = "linux")]
const PROPS: &str = "Id,Description,LoadState,ActiveState,SubState,MainPID,UnitFileState,User,ExecStart,FragmentPath,ExecMainStatus";

#[cfg(target_os = "linux")]
pub fn read() -> super::Services {
    use super::{Note, Services};

    let files = match systemctl(&["list-unit-files", "--type=service", "--no-legend", "--no-pager"]) {
        Ok(s) => parse_files(&s),
        Err(true) => return Services { list: Vec::new(), source: "systemctl", note: Some(Note::NoSystemctl) },
        Err(false) => Vec::new(),
    };
    // systemd marks itself running with this directory (sd_booted).
    if !std::path::Path::new("/run/systemd/system").exists() {
        return Services { list: merge(Vec::new(), &files), source: "unit files", note: Some(Note::NotRunning) };
    }
    let shown = systemctl(&["show", "--no-pager", &format!("--property={PROPS}"), "*.service"]).map(|s| parse_show(&s)).unwrap_or_default();
    Services { list: merge(shown, &files), source: "systemd", note: None }
}

/// Its output; `Err(true)` when there is no `systemctl` at all.
#[cfg(target_os = "linux")]
fn systemctl(args: &[&str]) -> Result<String, bool> {
    use std::process::{Command, Stdio};

    let out = Command::new("systemctl")
        .args(args)
        .env("LC_ALL", "C")
        .env("SYSTEMD_COLORS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| e.kind() == std::io::ErrorKind::NotFound)?;
    if !out.status.success() {
        return Err(false);
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn state(active: &str) -> State {
    match active {
        "active" | "reloading" | "refreshing" => State::Running,
        "activating" => State::Starting,
        "deactivating" => State::Stopping,
        "inactive" => State::Stopped,
        "failed" => State::Failed,
        _ => State::Unknown,
    }
}

pub fn start(file_state: &str) -> Start {
    match file_state {
        "enabled" | "enabled-runtime" => Start::Auto,
        "disabled" | "masked" | "masked-runtime" | "bad" => Start::Disabled,
        "static" | "indirect" | "generated" | "transient" | "linked" | "linked-runtime" | "alias" => Start::Manual,
        _ => Start::Unknown,
    }
}

/// The first command line of an `ExecStart=` value:
/// `{ path=/usr/sbin/cron ; argv[]=/usr/sbin/cron -f ; ignore_errors=no ; … }`.
pub fn argv(exec: &str) -> Option<String> {
    let at = exec.find("argv[]=")? + "argv[]=".len();
    let rest = &exec[at..];
    let end = rest.find(" ;").unwrap_or(rest.len());
    let s = rest[..end].trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// `systemctl show`'s blocks, one per unit, apart by a blank line. Units
/// that are referenced but have no file (`LoadState=not-found`) are left out.
pub fn parse_show(s: &str) -> Vec<Service> {
    let mut out = Vec::new();
    for block in s.split("\n\n") {
        let mut v = Service::default();
        let mut load = "";
        for line in block.lines() {
            let Some((k, val)) = line.split_once('=') else { continue };
            match k {
                "Id" => v.name = val.to_string(),
                "Description" => v.display = val.to_string(),
                "LoadState" => load = val,
                "ActiveState" => v.state = state(val),
                "SubState" => v.sub = val.to_string(),
                "MainPID" => v.pid = val.parse().ok().filter(|&p| p > 0),
                "UnitFileState" => {
                    v.start = start(val);
                    v.start_raw = val.to_string();
                }
                "User" if !val.is_empty() => v.user = Some(val.to_string()),
                "ExecStart" => v.cmd = argv(val),
                "FragmentPath" if !val.is_empty() => v.file = Some(val.to_string()),
                "ExecMainStatus" => v.exit = val.parse().ok(),
                _ => {}
            }
        }
        // Only a failed unit's status says something; the order of the
        // properties is systemctl's, so look again once all are read.
        if v.state != State::Failed {
            v.exit = None;
        }
        if !v.name.is_empty() && load != "not-found" {
            out.push(v);
        }
    }
    out
}

/// `list-unit-files`' rows: the unit and its state. Templates (`getty@.service`)
/// are no service by themselves, and an alias is the unit it names.
pub fn parse_files(s: &str) -> Vec<(String, String)> {
    s.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let (unit, st) = (it.next()?, it.next()?);
            (unit.ends_with(".service") && !unit.ends_with("@.service") && st != "alias").then(|| (unit.to_string(), st.to_string()))
        })
        .collect()
}

/// The loaded units, and the unit files not among them as stopped (or
/// unknown when nothing was loaded: systemd is not running then).
pub fn merge(mut shown: Vec<Service>, files: &[(String, String)]) -> Vec<Service> {
    let live = !shown.is_empty();
    let known: std::collections::HashSet<String> = shown.iter().map(|s| s.name.clone()).collect();
    for (unit, st) in files {
        if known.contains(unit) {
            continue;
        }
        shown.push(Service {
            name: unit.clone(),
            state: if live { State::Stopped } else { State::Unknown },
            sub: if live { "dead".to_string() } else { String::new() },
            start: start(st),
            start_raw: st.clone(),
            ..Service::default()
        });
    }
    shown
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHOW: &str = "\
Id=cron.service
Description=Regular background program processing daemon
LoadState=loaded
ActiveState=active
SubState=running
MainPID=650
ExecMainStatus=0
UnitFileState=enabled
User=
ExecStart={ path=/usr/sbin/cron ; argv[]=/usr/sbin/cron -f -P ; ignore_errors=no ; start_time=[Fri 2026-10-09] ; stop_time=[n/a] ; pid=650 ; code=(null) ; status=0/0 }
FragmentPath=/usr/lib/systemd/system/cron.service

ExecMainStatus=1
Id=broken.service
Description=Broken
LoadState=loaded
ActiveState=failed
SubState=failed
MainPID=0
UnitFileState=static
User=nobody
ExecStart=
FragmentPath=/etc/systemd/system/broken.service

Id=gone.service
Description=gone.service
LoadState=not-found
ActiveState=inactive
SubState=dead
MainPID=0
UnitFileState=

Id=oneshot.service
LoadState=loaded
ActiveState=active
SubState=exited
MainPID=0
ExecMainStatus=0
UnitFileState=enabled
";

    #[test]
    fn show_blocks_become_services() {
        let v = parse_show(SHOW);
        assert_eq!(v.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["cron.service", "broken.service", "oneshot.service"]);
        let cron = &v[0];
        assert_eq!((cron.state, cron.sub.as_str(), cron.pid), (State::Running, "running", Some(650)));
        assert_eq!((cron.start, cron.start_raw.as_str()), (Start::Auto, "enabled"));
        assert_eq!(cron.cmd.as_deref(), Some("/usr/sbin/cron -f -P"));
        assert_eq!((cron.user.as_deref(), cron.exit), (None, None));
        assert_eq!(cron.file.as_deref(), Some("/usr/lib/systemd/system/cron.service"));
        let broken = &v[1];
        assert_eq!((broken.state, broken.pid, broken.exit), (State::Failed, None, Some(1)));
        assert_eq!((broken.start, broken.user.as_deref(), broken.cmd.as_deref()), (Start::Manual, Some("nobody"), None));
        assert_eq!((v[2].state, v[2].sub.as_str()), (State::Running, "exited"));
    }

    #[test]
    fn unit_files_skip_templates_and_aliases_and_fill_in_what_is_not_loaded() {
        let files = parse_files(
            "apt-daily.service                            static   -\n\
             autovt@.service                              alias    -\n\
             getty@.service                               enabled  enabled\n\
             dbus-org.freedesktop.timedate1.service       alias    -\n\
             console-getty.service                        disabled disabled\n\
             cron.service                                 enabled  enabled\n\
             cron.timer                                   enabled  enabled\n",
        );
        let names: Vec<&str> = files.iter().map(|(u, _)| u.as_str()).collect();
        assert_eq!(names, ["apt-daily.service", "console-getty.service", "cron.service"]);

        // systemd running: what is not loaded is stopped.
        let v = merge(parse_show(SHOW), &files);
        assert_eq!(v.len(), 5);
        let cg = v.iter().find(|s| s.name == "console-getty.service").unwrap();
        assert_eq!((cg.state, cg.start, cg.sub.as_str()), (State::Stopped, Start::Disabled, "dead"));
        assert_eq!(v.iter().filter(|s| s.name == "cron.service").count(), 1);

        // Not running: every unit is there, its state unknown.
        let v = merge(Vec::new(), &files);
        assert!(v.iter().all(|s| s.state == State::Unknown && s.sub.is_empty()));
        assert_eq!(v[0].start, Start::Manual);
    }

    #[test]
    fn states_and_starts_fold_systemds_words() {
        assert_eq!(state("activating"), State::Starting);
        assert_eq!(state("deactivating"), State::Stopping);
        assert_eq!(state("inactive"), State::Stopped);
        assert_eq!(state("maintenance"), State::Unknown);
        assert_eq!(start("masked"), Start::Disabled);
        assert_eq!(start("indirect"), Start::Manual);
        assert_eq!(start(""), Start::Unknown);
        assert_eq!(argv("{ path=/bin/x ; argv[]=/bin/x ; ignore_errors=no }").as_deref(), Some("/bin/x"));
        assert_eq!(argv(""), None);
    }
}
