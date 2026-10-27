//! Containers: what Docker or Podman runs, with each one's CPU, memory and
//! processes. Asked of the engine's own API (Docker's, which Podman speaks
//! too) over its socket: a Unix socket on Linux and macOS, a named pipe on
//! Windows. Nothing is linked: the request is a few lines of HTTP and the
//! answer JSON. Read only while the screen shows; a machine without an
//! engine says so rather than showing an empty list. Starting, stopping and
//! restarting one is [`act`].
//!
//! The parsing and the CPU arithmetic are pure functions so the tests run
//! on every OS.

pub mod act;

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How long one read of the engine may take before it is given up on.
const PATIENCE: Duration = Duration::from_secs(8);

/// Which engine answered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Runtime {
    #[default]
    Docker,
    Podman,
}

impl Runtime {
    pub fn key(self) -> &'static str {
        match self {
            Runtime::Docker => "ct.docker",
            Runtime::Podman => "ct.podman",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum State {
    Running,
    Paused,
    Restarting,
    Created,
    Exited,
    #[default]
    Other,
}

impl State {
    fn parse(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "running" => State::Running,
            "paused" => State::Paused,
            "restarting" => State::Restarting,
            "created" | "configured" | "initialized" => State::Created,
            "exited" | "stopped" | "dead" | "removing" => State::Exited,
            _ => State::Other,
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            State::Running => "ct.state.running",
            State::Paused => "ct.state.paused",
            State::Restarting => "ct.state.restarting",
            State::Created => "ct.state.created",
            State::Exited => "ct.state.exited",
            State::Other => "ct.state.other",
        }
    }

    /// Whether it is using the machine now.
    pub fn live(self) -> bool {
        matches!(self, State::Running | State::Paused | State::Restarting)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Container {
    /// The first 12 characters, as `docker ps` shows.
    pub id: String,
    pub name: String,
    pub image: String,
    pub state: State,
    /// The engine's own words: `Up 3 hours (healthy)`.
    pub status: String,
    /// Percent of one logical CPU, as `docker stats` does: two busy CPUs
    /// read 200. `None` until two readings are in, or where it is not said.
    pub cpu: Option<f32>,
    /// Resident bytes without the file cache, as `docker stats` counts them.
    pub mem: Option<u64>,
    pub mem_limit: Option<u64>,
    pub pids: Option<u32>,
    /// Seconds since the epoch.
    pub created: u64,
}

impl Container {
    /// Restarting, or exited with a non-zero code; a clean exit is a
    /// container that did its job, not a warning.
    pub fn problem(&self) -> bool {
        match self.state {
            State::Restarting => true,
            State::Exited => {
                let code = self.status.strip_prefix("Exited (").and_then(|r| r.split(')').next());
                code.is_some_and(|c| c.trim() != "0")
            }
            _ => false,
        }
    }
}

/// Why the list is empty or short.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Note {
    /// No engine is listening where one usually is.
    NoEngine,
    /// The socket is there and this user may not use it.
    Denied,
    /// It did not answer in time, or not in a way that could be read.
    Failed,
}

impl Note {
    pub fn key(self) -> &'static str {
        match self {
            Note::NoEngine => "ct.note.none",
            Note::Denied => "ct.note.denied",
            Note::Failed => "ct.note.failed",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Containers {
    pub list: Vec<Container>,
    pub runtime: Option<Runtime>,
    pub note: Option<Note>,
    /// Where the list was read: what is done to one of its containers goes
    /// there and nowhere else. Not sent to another machine.
    #[serde(skip)]
    pub engine: Option<Endpoint>,
}

/// Where an engine listens.
#[derive(Clone, Debug, PartialEq)]
pub enum Endpoint {
    #[cfg_attr(not(unix), allow(dead_code))]
    Unix(PathBuf),
    #[cfg_attr(not(windows), allow(dead_code))]
    Pipe(String),
}

/// One CPU reading of a container.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Sample {
    total: u64,
    system: u64,
    online: u32,
    pre_total: u64,
    pre_system: u64,
    mem: Option<u64>,
    limit: Option<u64>,
    pids: Option<u32>,
}

/// Reads containers; keeps the last CPU counters of each so a rate is the
/// difference between two readings.
#[derive(Default)]
pub struct Reader {
    engine: Option<(Runtime, Endpoint)>,
    prev: HashMap<String, (u64, u64)>,
}

impl Reader {
    /// Ask the engine, on a thread of its own so a stuck pipe cannot hold
    /// the sampler.
    pub fn read(&mut self) -> Containers {
        let (tx, rx) = std::sync::mpsc::channel();
        let (engine, prev) = (self.engine.clone(), std::mem::take(&mut self.prev));
        let started = std::thread::Builder::new().name("yagura-containers".into()).spawn(move || {
            let _ = tx.send(read_now(engine, prev));
        });
        if started.is_err() {
            return Containers { note: Some(Note::Failed), ..Containers::default() };
        }
        match rx.recv_timeout(PATIENCE) {
            Ok((out, engine, prev)) => {
                self.engine = engine;
                self.prev = prev;
                out
            }
            Err(_) => Containers { note: Some(Note::Failed), ..Containers::default() },
        }
    }
}

type Counters = HashMap<String, (u64, u64)>;

fn read_now(engine: Option<(Runtime, Endpoint)>, prev: Counters) -> (Containers, Option<(Runtime, Endpoint)>, Counters) {
    let mut denied = false;
    let found = engine.and_then(|(rt, ep)| list(&ep).ok().map(|l| (rt, ep, l))).or_else(|| {
        for (rt, ep) in candidates() {
            match list(&ep) {
                Ok(l) => return Some((rt, ep, l)),
                Err(Fail::Denied) => denied = true,
                Err(_) => {}
            }
        }
        None
    });
    let Some((rt, ep, mut list)) = found else {
        let note = if denied { Note::Denied } else { Note::NoEngine };
        return (Containers { note: Some(note), ..Containers::default() }, None, Counters::new());
    };
    // Every running one's stats at once: each takes a moment to answer.
    let samples: Vec<Option<Sample>> = std::thread::scope(|s| {
        let jobs: Vec<_> = list
            .iter()
            .map(|c| {
                let (live, id, ep) = (c.state == State::Running, c.id.clone(), &ep);
                s.spawn(move || live.then(|| stats(ep, &id)).flatten())
            })
            .collect();
        jobs.into_iter().map(|j| j.join().ok().flatten()).collect()
    });
    let mut next = Counters::new();
    for (c, s) in list.iter_mut().zip(samples) {
        let Some(s) = s else { continue };
        c.cpu = cpu_pct(&s, prev.get(&c.id).copied());
        c.mem = s.mem;
        c.mem_limit = s.limit;
        c.pids = s.pids;
        next.insert(c.id.clone(), (s.total, s.system));
    }
    (Containers { list, runtime: Some(rt), note: None, engine: Some(ep.clone()) }, Some((rt, ep)), next)
}

/// `docker stats`' formula: the container's CPU time over the machine's,
/// times the CPUs. With no earlier reading of ours the engine's own
/// `precpu_stats` is the other end, when it filled them.
fn cpu_pct(s: &Sample, prev: Option<(u64, u64)>) -> Option<f32> {
    let (t0, s0) = prev.or((s.pre_system > 0).then_some((s.pre_total, s.pre_system)))?;
    let (dt, ds) = (s.total.checked_sub(t0)?, s.system.checked_sub(s0)?);
    (ds > 0).then(|| (dt as f64 / ds as f64 * f64::from(s.online.max(1)) * 100.0) as f32)
}

fn u(v: &Value, ptr: &str) -> Option<u64> {
    v.pointer(ptr).and_then(Value::as_u64)
}

fn parse_stats(body: &str) -> Option<Sample> {
    let v: Value = serde_json::from_str(body).ok()?;
    // Without the file cache: v2 calls it `inactive_file`, v1 `total_inactive_file`.
    let cache = u(&v, "/memory_stats/stats/inactive_file").or_else(|| u(&v, "/memory_stats/stats/total_inactive_file")).unwrap_or(0);
    Some(Sample {
        total: u(&v, "/cpu_stats/cpu_usage/total_usage").unwrap_or(0),
        system: u(&v, "/cpu_stats/system_cpu_usage").unwrap_or(0),
        online: u(&v, "/cpu_stats/online_cpus").unwrap_or(1) as u32,
        pre_total: u(&v, "/precpu_stats/cpu_usage/total_usage").unwrap_or(0),
        pre_system: u(&v, "/precpu_stats/system_cpu_usage").unwrap_or(0),
        mem: u(&v, "/memory_stats/usage").map(|m| m.saturating_sub(cache)),
        limit: u(&v, "/memory_stats/limit").filter(|&l| l > 0),
        pids: u(&v, "/pids_stats/current").map(|p| p as u32),
    })
}

fn parse_list(body: &str) -> Option<Vec<Container>> {
    let v: Value = serde_json::from_str(body).ok()?;
    let mut out: Vec<Container> = v
        .as_array()?
        .iter()
        .map(|c| {
            let text = |k: &str| c.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            let name = c.get("Names").and_then(Value::as_array).and_then(|n| n.first()).and_then(Value::as_str).unwrap_or("");
            let id = text("Id");
            Container {
                id: id.chars().take(12).collect(),
                name: name.trim_start_matches('/').to_string(),
                image: text("Image"),
                state: State::parse(&text("State")),
                status: text("Status"),
                created: c.get("Created").and_then(Value::as_u64).unwrap_or(0),
                ..Container::default()
            }
        })
        .collect();
    // The running first, each group by name.
    out.sort_by(|a, b| b.state.live().cmp(&a.state.live()).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Some(out)
}

#[derive(Debug, PartialEq)]
enum Fail {
    Denied,
    Other,
}

/// A response's status code and body: chunked bodies put together.
fn parse_http(raw: &[u8]) -> Option<(u16, String)> {
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&raw[..split]).ok()?;
    let code = head.lines().next()?.split_whitespace().nth(1)?.parse().ok()?;
    let body = &raw[split + 4..];
    let chunked = head.lines().any(|l| l.to_lowercase().starts_with("transfer-encoding:") && l.to_lowercase().contains("chunked"));
    let bytes = if chunked { dechunk(body)? } else { body.to_vec() };
    Some((code, String::from_utf8_lossy(&bytes).into_owned()))
}

fn dechunk(mut b: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let eol = b.windows(2).position(|w| w == b"\r\n")?;
        let size = usize::from_str_radix(std::str::from_utf8(&b[..eol]).ok()?.split(';').next()?.trim(), 16).ok()?;
        b = &b[eol + 2..];
        if size == 0 {
            return Some(out);
        }
        out.extend_from_slice(b.get(..size)?);
        b = b.get(size + 2..)?;
    }
}

fn get(ep: &Endpoint, path: &str) -> Result<String, Fail> {
    let raw = request(ep, "GET", path, PATIENCE)
        .map_err(|e| if e.kind() == std::io::ErrorKind::PermissionDenied { Fail::Denied } else { Fail::Other })?;
    match parse_http(&raw) {
        Some((200, body)) => Ok(body),
        _ => Err(Fail::Other),
    }
}

fn list(ep: &Endpoint) -> Result<Vec<Container>, Fail> {
    parse_list(&get(ep, "/containers/json?all=1")?).ok_or(Fail::Other)
}

fn stats(ep: &Endpoint, id: &str) -> Option<Sample> {
    parse_stats(&get(ep, &format!("/containers/{id}/stats?stream=false&one-shot=true")).ok()?)
}

fn http(method: &str, path: &str) -> String {
    // 1.0 so the engine neither chunks nor keeps the connection. A POST says
    // it has no body, which an engine may want to hear.
    let len = if method == "GET" { "" } else { "Content-Length: 0\r\n" };
    format!("{method} {path} HTTP/1.0\r\nHost: engine\r\nAccept: application/json\r\nUser-Agent: yagura\r\n{len}\r\n")
}

fn exchange(mut s: impl Read + Write, method: &str, path: &str) -> std::io::Result<Vec<u8>> {
    s.write_all(http(method, path).as_bytes())?;
    let mut out = Vec::new();
    s.read_to_end(&mut out)?;
    Ok(out)
}

/// A connection to the engine. A read or write that waits longer than
/// `patience` fails; a named pipe has no such limit, so whoever must not wait
/// for ever asks on a thread of its own.
#[cfg(unix)]
fn open(ep: &Endpoint, patience: Duration) -> std::io::Result<std::os::unix::net::UnixStream> {
    let Endpoint::Unix(p) = ep else { return Err(std::io::ErrorKind::Unsupported.into()) };
    let s = std::os::unix::net::UnixStream::connect(p)?;
    s.set_read_timeout(Some(patience))?;
    s.set_write_timeout(Some(patience))?;
    Ok(s)
}

#[cfg(windows)]
fn open(ep: &Endpoint, _patience: Duration) -> std::io::Result<std::fs::File> {
    let Endpoint::Pipe(p) = ep else { return Err(std::io::ErrorKind::Unsupported.into()) };
    std::fs::OpenOptions::new().read(true).write(true).open(p)
}

#[cfg(not(any(unix, windows)))]
fn open(_ep: &Endpoint, _patience: Duration) -> std::io::Result<std::fs::File> {
    Err(std::io::ErrorKind::Unsupported.into())
}

/// One request and its whole answer.
fn request(ep: &Endpoint, method: &str, path: &str, patience: Duration) -> std::io::Result<Vec<u8>> {
    exchange(open(ep, patience)?, method, path)
}

/// `DOCKER_HOST`'s `unix://` or `npipe://` form.
fn from_host(h: &str) -> Option<Endpoint> {
    if let Some(p) = h.strip_prefix("unix://") {
        return Some(Endpoint::Unix(PathBuf::from(p)));
    }
    let p = h.strip_prefix("npipe://")?;
    Some(Endpoint::Pipe(p.replace('/', "\\")))
}

/// The places an engine usually listens, the one `DOCKER_HOST` names first.
#[cfg(unix)]
fn candidates() -> Vec<(Runtime, Endpoint)> {
    let mut v = Vec::new();
    if let Some(ep) = std::env::var("DOCKER_HOST").ok().and_then(|h| from_host(&h)) {
        v.push((Runtime::Docker, ep));
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let run = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    v.push((Runtime::Docker, Endpoint::Unix("/var/run/docker.sock".into())));
    if let Some(r) = &run {
        v.push((Runtime::Docker, Endpoint::Unix(r.join("docker.sock"))));
        v.push((Runtime::Podman, Endpoint::Unix(r.join("podman/podman.sock"))));
    }
    v.push((Runtime::Podman, Endpoint::Unix("/run/podman/podman.sock".into())));
    if let Some(h) = &home {
        v.push((Runtime::Docker, Endpoint::Unix(h.join(".docker/run/docker.sock"))));
    }
    v
}

#[cfg(windows)]
fn candidates() -> Vec<(Runtime, Endpoint)> {
    let mut v = Vec::new();
    if let Some(ep) = std::env::var("DOCKER_HOST").ok().and_then(|h| from_host(&h)) {
        v.push((Runtime::Docker, ep));
    }
    v.push((Runtime::Docker, Endpoint::Pipe(r"\\.\pipe\docker_engine".into())));
    v.push((Runtime::Podman, Endpoint::Pipe(r"\\.\pipe\podman-machine-default".into())));
    v
}

#[cfg(not(any(unix, windows)))]
fn candidates() -> Vec<(Runtime, Endpoint)> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_container_warns_when_it_restarts_or_exits_badly() {
        let c = |state, status: &str| Container { state, status: status.into(), ..Default::default() };
        assert!(c(State::Restarting, "Restarting (1) 5 seconds ago").problem());
        assert!(c(State::Exited, "Exited (137) 2 hours ago").problem());
        assert!(!c(State::Exited, "Exited (0) 2 hours ago").problem());
        assert!(!c(State::Exited, "Exited").problem());
        assert!(!c(State::Running, "Up 3 hours").problem());
    }

    use super::*;

    const LIST: &str = r#"[
        {"Id":"b3f1c2d4e5f60718293a4b5c6d7e8f90","Names":["/web"],"Image":"nginx:1.27","State":"running","Status":"Up 3 hours (healthy)","Created":1760000000},
        {"Id":"0123456789abcdef0123","Names":["db"],"Image":"postgres","State":"exited","Status":"Exited (0) 2 days ago","Created":1750000000},
        {"Id":"aaaaaaaaaaaaaaaa","Names":["/app"],"Image":"app:dev","State":"paused","Status":"Up (Paused)","Created":1}
    ]"#;

    const STATS: &str = r#"{
        "cpu_stats":{"cpu_usage":{"total_usage":2000000000},"system_cpu_usage":40000000000,"online_cpus":4},
        "precpu_stats":{"cpu_usage":{"total_usage":1000000000},"system_cpu_usage":36000000000},
        "memory_stats":{"usage":300000000,"limit":1000000000,"stats":{"inactive_file":100000000}},
        "pids_stats":{"current":7}
    }"#;

    #[test]
    fn the_list_puts_the_live_first_by_name() {
        let l = parse_list(LIST).unwrap();
        let names: Vec<&str> = l.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["app", "web", "db"]);
        assert_eq!(l[1].id, "b3f1c2d4e5f6");
        assert_eq!(l[1].state, State::Running);
        assert_eq!(l[2].state, State::Exited);
        assert_eq!(l[0].state, State::Paused);
        assert!(parse_list("{}").is_none());
    }

    #[test]
    fn stats_leave_out_the_file_cache() {
        let s = parse_stats(STATS).unwrap();
        assert_eq!(s.mem, Some(200_000_000));
        assert_eq!(s.limit, Some(1_000_000_000));
        assert_eq!(s.pids, Some(7));
        assert_eq!(s.online, 4);
    }

    #[test]
    fn cpu_is_the_share_of_the_machines_time_times_the_cpus() {
        let s = parse_stats(STATS).unwrap();
        // The engine's own earlier reading: 1s of 4s, on 4 CPUs.
        let own = cpu_pct(&s, None).unwrap();
        assert!((own - 100.0).abs() < 0.01, "{own}");
        // Ours: 0.5s of 2s.
        let ours = cpu_pct(&s, Some((1_500_000_000, 38_000_000_000))).unwrap();
        assert!((ours - 100.0).abs() < 0.01, "{ours}");
        // Nothing earlier, or time that did not move, says nothing.
        assert_eq!(cpu_pct(&Sample::default(), None), None);
        assert_eq!(cpu_pct(&s, Some((s.total, s.system))), None);
    }

    #[test]
    fn http_answers_are_split_and_dechunked() {
        let plain = b"HTTP/1.0 200 OK\r\nContent-Type: application/json\r\n\r\n[1]";
        assert_eq!(parse_http(plain), Some((200, "[1]".into())));
        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\n[1,\r\n2\r\n2]\r\n0\r\n\r\n";
        assert_eq!(parse_http(chunked), Some((200, "[1,2]".into())));
        let down = b"HTTP/1.0 404 Not Found\r\n\r\n{}";
        assert_eq!(parse_http(down).map(|r| r.0), Some(404));
        assert_eq!(parse_http(b"garbage"), None);
    }

    /// The stream an exchange runs on: what was written, and an answer to read.
    struct Fake {
        got: Vec<u8>,
        answer: std::io::Cursor<Vec<u8>>,
    }

    impl Read for Fake {
        fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
            self.answer.read(b)
        }
    }

    impl Write for Fake {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.got.write(b)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_post_says_it_has_no_body_and_a_get_does_not() {
        let get = http("GET", "/containers/json?all=1");
        assert!(get.starts_with("GET /containers/json?all=1 HTTP/1.0\r\n"), "{get}");
        assert!(!get.contains("Content-Length"), "{get}");
        assert!(get.ends_with("\r\n\r\n"), "{get}");
        let post = http("POST", "/containers/web/stop");
        assert!(post.starts_with("POST /containers/web/stop HTTP/1.0\r\n"), "{post}");
        assert!(post.ends_with("\r\nContent-Length: 0\r\n\r\n"), "{post}");
        let mut f = Fake { got: Vec::new(), answer: std::io::Cursor::new(b"HTTP/1.0 204 No Content\r\n\r\n".to_vec()) };
        let raw = exchange(&mut f, "POST", "/containers/web/stop").unwrap();
        assert_eq!(parse_http(&raw), Some((204, String::new())));
        assert_eq!(String::from_utf8(f.got).unwrap(), post);
    }

    #[test]
    fn docker_host_names_an_endpoint() {
        assert_eq!(from_host("unix:///run/user/1000/docker.sock"), Some(Endpoint::Unix("/run/user/1000/docker.sock".into())));
        assert_eq!(from_host("npipe:////./pipe/docker_engine"), Some(Endpoint::Pipe(r"\\.\pipe\docker_engine".into())));
        assert_eq!(from_host("tcp://1.2.3.4:2375"), None);
    }

    #[test]
    fn states_come_from_either_engines_words() {
        assert_eq!(State::parse("Running"), State::Running);
        assert_eq!(State::parse("stopped"), State::Exited);
        assert_eq!(State::parse("configured"), State::Created);
        assert!(State::Paused.live() && !State::Exited.live());
    }
}
