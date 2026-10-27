//! Starting, stopping and restarting a container: the confirmed `S`, `x` and
//! `R` on the containers screen, on this machine only. Each is one
//! `POST /containers/{id}/start` (or `stop`, `restart`) to the engine that
//! listed it, on a thread of its own, and the outcome comes back through a
//! channel, waking the UI, as for services.

use std::io::ErrorKind;
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

use crossbeam_channel::Sender;
use serde_json::Value;

use super::{Endpoint, Runtime, State, candidates, exchange, open, parse_http};
use crate::svc::Action;

/// How long one request may take: a stop waits out the container's own stop
/// timeout (10 s unless it was given another) before the engine kills it.
const PATIENCE: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    pub action: Action,
    /// As the list has it: the first 12 characters, which the engine takes.
    pub id: String,
    /// Its name, or its id when it has none.
    pub name: String,
    pub runtime: Runtime,
    /// The engine that listed it; `None` asks each engine of its kind in turn.
    pub engine: Option<Endpoint>,
}

impl Target {
    /// What the engine knows it by: its id, or its name when the list had none.
    pub fn key(&self) -> &str {
        if self.id.is_empty() { &self.name } else { &self.id }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Done,
    /// It already was where it was asked to go.
    Already,
    /// No engine knows it any more.
    Gone,
    /// The engine is there and this user may not use it.
    Denied,
    /// No engine of its kind answered.
    NoEngine,
    /// Asked, and no answer in time: it may still finish.
    Slow,
    /// Asked, and the answer was cut off or could not be read: it may have
    /// happened.
    Unclear,
    /// The engine refused, in its own words.
    Failed(String),
}

/// Whether asking makes sense: a running one is not started again, one that
/// is not running not stopped. Restart is asked in any state.
pub fn fits(action: Action, state: State) -> bool {
    match action {
        Action::Start => !state.live(),
        Action::Stop => state.live(),
        Action::Restart => true,
    }
}

fn verb(a: Action) -> &'static str {
    match a {
        Action::Start => "start",
        Action::Stop => "stop",
        Action::Restart => "restart",
    }
}

fn tool(r: Runtime) -> &'static str {
    match r {
        Runtime::Docker => "docker",
        Runtime::Podman => "podman",
    }
}

/// The command that does the same, for the question: `docker stop web`.
pub fn how(t: &Target) -> String {
    format!("{} {} {}", tool(t.runtime), verb(t.action), t.name)
}

/// The command that resumes a paused one.
pub fn unpause(t: &Target) -> String {
    format!("{} unpause {}", tool(t.runtime), t.name)
}

/// Run `t` on a thread of its own; the outcome comes back on `tx`. The
/// engine is asked on yet another, so one that never answers (a named pipe
/// has no timeout) is reported as slow rather than leaving the toast waiting.
pub fn spawn(t: Target, tx: Sender<(Target, Outcome)>, wake: crate::Wake) {
    let (t2, tx2) = (t.clone(), tx.clone());
    let started = std::thread::Builder::new().name("yagura-ct".into()).spawn(move || {
        let (otx, orx) = std::sync::mpsc::channel();
        let asked = t.clone();
        let o = match std::thread::Builder::new().name("yagura-ct-engine".into()).spawn(move || {
            let engines = asked.engine.clone().map_or_else(|| of_kind(asked.runtime, candidates()), |e| vec![e]);
            let _ = otx.send(run(&asked, engines, PATIENCE));
        }) {
            Ok(_) => match orx.recv_timeout(PATIENCE + Duration::from_secs(1)) {
                Ok(o) => o,
                Err(RecvTimeoutError::Timeout) => Outcome::Slow,
                // The asking thread died midway: whether it asked is not known.
                Err(RecvTimeoutError::Disconnected) => Outcome::Unclear,
            },
            Err(e) => Outcome::Failed(e.to_string()),
        };
        let _ = tx.send((t, o));
        if let Some(w) = wake {
            w();
        }
    });
    if let Err(e) = started {
        let _ = tx2.send((t2, Outcome::Failed(e.to_string())));
    }
}

/// The engines of `rt`'s kind among `all`, in the order they are looked
/// for, each once (`DOCKER_HOST` may name the usual one again).
fn of_kind(rt: Runtime, all: Vec<(Runtime, Endpoint)>) -> Vec<Endpoint> {
    let mut out: Vec<Endpoint> = Vec::new();
    for (r, ep) in all {
        if r == rt && !out.contains(&ep) {
            out.push(ep);
        }
    }
    out
}

/// Ask `engines` in turn until one knows `t`. One that cannot be reached is
/// passed over; once the question is sent, its answer is the outcome, so a
/// container is never asked twice.
fn run(t: &Target, engines: Vec<Endpoint>, patience: Duration) -> Outcome {
    let path = format!("/containers/{}/{}", t.key(), verb(t.action));
    let (mut denied, mut unknown) = (false, false);
    for ep in engines {
        let s = match open(&ep, patience) {
            Ok(s) => s,
            Err(e) if e.kind() == ErrorKind::PermissionDenied => {
                denied = true;
                continue;
            }
            Err(_) => continue,
        };
        let raw = match exchange(s, "POST", &path) {
            Ok(raw) => raw,
            // Sent, and the engine is still at it.
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => return Outcome::Slow,
            Err(_) => return Outcome::Unclear,
        };
        let Some((code, body)) = parse_http(&raw) else { return Outcome::Unclear };
        match outcome(code, &body) {
            Some(o) => return o,
            None => unknown = true,
        }
    }
    // One that would not let this user ask may be the one that has it.
    if denied {
        Outcome::Denied
    } else if unknown {
        Outcome::Gone
    } else {
        Outcome::NoEngine
    }
}

/// What an engine's answer means; `None` when it does not know the
/// container, so another engine may.
fn outcome(code: u16, body: &str) -> Option<Outcome> {
    match code {
        200..=299 => Some(Outcome::Done),
        304 => Some(Outcome::Already),
        404 => None,
        _ => {
            let said = serde_json::from_str::<Value>(body).ok().and_then(|v| Some(v.get("message")?.as_str()?.trim().to_string()));
            Some(Outcome::Failed(said.filter(|s| !s.is_empty()).unwrap_or_else(|| format!("HTTP {code}"))))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(action: Action) -> Target {
        Target { action, id: "b3f1c2d4e5f6".into(), name: "web".into(), runtime: Runtime::Docker, engine: None }
    }

    /// Where nothing listens.
    fn nowhere(p: &str) -> Endpoint {
        if cfg!(windows) {
            Endpoint::Pipe(format!(r"\\.\pipe\yagura-test-{p}"))
        } else {
            Endpoint::Unix(format!("/nonexistent/yagura-test-{p}").into())
        }
    }

    #[test]
    fn only_a_move_somewhere_else_is_asked() {
        assert!(fits(Action::Start, State::Exited));
        assert!(fits(Action::Start, State::Created));
        assert!(!fits(Action::Start, State::Running));
        assert!(!fits(Action::Start, State::Paused));
        assert!(fits(Action::Stop, State::Running));
        assert!(fits(Action::Stop, State::Paused));
        assert!(fits(Action::Stop, State::Restarting));
        assert!(!fits(Action::Stop, State::Exited));
        assert!(fits(Action::Restart, State::Exited));
        assert!(fits(Action::Restart, State::Running));
    }

    #[test]
    fn the_command_names_the_engine_and_the_container() {
        assert_eq!(how(&target(Action::Stop)), "docker stop web");
        let t = Target { runtime: Runtime::Podman, ..target(Action::Restart) };
        assert_eq!(how(&t), "podman restart web");
        assert_eq!(unpause(&t), "podman unpause web");
    }

    #[test]
    fn the_engine_knows_it_by_its_id_or_else_its_name() {
        assert_eq!(target(Action::Stop).key(), "b3f1c2d4e5f6");
        assert_eq!(Target { id: String::new(), ..target(Action::Stop) }.key(), "web");
    }

    #[test]
    fn the_engines_answer_says_what_happened() {
        assert_eq!(outcome(204, ""), Some(Outcome::Done));
        assert_eq!(outcome(304, ""), Some(Outcome::Already));
        assert_eq!(outcome(404, r#"{"message":"No such container: web"}"#), None);
        let said = r#"{"message":"driver failed programming external connectivity: port is already allocated\n"}"#;
        assert_eq!(
            outcome(500, said),
            Some(Outcome::Failed("driver failed programming external connectivity: port is already allocated".into()))
        );
        assert_eq!(outcome(409, "not json"), Some(Outcome::Failed("HTTP 409".into())));
        assert_eq!(outcome(500, r#"{"message":"  "}"#), Some(Outcome::Failed("HTTP 500".into())));
    }

    #[test]
    fn only_engines_of_its_kind_are_asked_each_once() {
        let (a, b, c) = (nowhere("a"), nowhere("b"), nowhere("c"));
        let all =
            vec![(Runtime::Docker, a.clone()), (Runtime::Podman, b.clone()), (Runtime::Docker, c.clone()), (Runtime::Docker, a.clone())];
        assert_eq!(of_kind(Runtime::Docker, all.clone()), vec![a, c]);
        assert_eq!(of_kind(Runtime::Podman, all), vec![b]);
    }

    #[test]
    fn no_engine_that_listens_is_no_engine() {
        assert_eq!(run(&target(Action::Start), vec![nowhere("a"), nowhere("b")], PATIENCE), Outcome::NoEngine);
        assert_eq!(run(&target(Action::Start), Vec::new(), PATIENCE), Outcome::NoEngine);
    }

    /// Real sockets answering as an engine does.
    #[cfg(unix)]
    mod socket {
        use std::io::{Read, Write};
        use std::os::unix::net::{UnixListener, UnixStream};
        use std::path::{Path, PathBuf};
        use std::thread::JoinHandle;
        use std::time::Instant;

        use super::*;

        const NOT_FOUND: &str = "HTTP/1.0 404 Not Found\r\nContent-Type: application/json\r\n\r\n{\"message\":\"No such container\"}";

        /// A short folder of its own: a socket's path must stay under 104
        /// bytes on macOS, which a deep temp folder may not.
        fn dir(test: &str) -> PathBuf {
            let d = PathBuf::from(format!("/tmp/yagura-ct-{}-{test}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            d
        }

        /// Takes one connection within 10 s, reads the request, waits `hold`,
        /// then writes `answer`; gives back the request, or `None` when
        /// nobody came.
        fn serve(p: &Path, answer: &'static str, hold: Duration) -> JoinHandle<Option<String>> {
            let l = UnixListener::bind(p).unwrap();
            l.set_nonblocking(true).unwrap();
            std::thread::spawn(move || {
                let until = Instant::now() + Duration::from_secs(10);
                let mut s = loop {
                    match l.accept() {
                        Ok((s, _)) => break s,
                        Err(_) if Instant::now() < until => std::thread::sleep(Duration::from_millis(10)),
                        Err(_) => return None,
                    }
                };
                s.set_nonblocking(false).unwrap();
                s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
                let mut got = Vec::new();
                let mut b = [0u8; 1024];
                while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut b) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => got.extend_from_slice(&b[..n]),
                    }
                }
                std::thread::sleep(hold);
                let _ = s.write_all(answer.as_bytes());
                Some(String::from_utf8_lossy(&got).into_owned())
            })
        }

        /// A listener nobody should call: `accept` fails while nobody did.
        fn quiet(p: &Path) -> UnixListener {
            let l = UnixListener::bind(p).unwrap();
            l.set_nonblocking(true).unwrap();
            l
        }

        #[test]
        fn a_post_goes_on_to_the_engine_that_knows_it() {
            let d = dir("next");
            let (a, b) = (d.join("a"), d.join("b"));
            let ha = serve(&a, NOT_FOUND, Duration::ZERO);
            let hb = serve(&b, "HTTP/1.0 204 No Content\r\n\r\n", Duration::ZERO);
            let o = run(&target(Action::Restart), vec![Endpoint::Unix(a), Endpoint::Unix(b)], PATIENCE);
            assert_eq!(o, Outcome::Done);
            for got in [ha.join().unwrap(), hb.join().unwrap()] {
                let got = got.expect("asked");
                assert!(got.starts_with("POST /containers/b3f1c2d4e5f6/restart HTTP/1.0\r\n"), "{got}");
                assert!(got.contains("\r\nContent-Length: 0\r\n"), "{got}");
            }
            let _ = std::fs::remove_dir_all(&d);
        }

        #[test]
        fn a_cut_off_answer_is_not_asked_again_elsewhere() {
            for answer in ["HTTP/1.0 20", ""] {
                let d = dir("cut");
                let (a, b) = (d.join("a"), d.join("b"));
                let ha = serve(&a, answer, Duration::ZERO);
                let lb = quiet(&b);
                let o = run(&target(Action::Stop), vec![Endpoint::Unix(a), Endpoint::Unix(b)], PATIENCE);
                assert_eq!(o, Outcome::Unclear, "{answer:?}");
                assert!(ha.join().unwrap().is_some());
                assert!(lb.accept().is_err(), "the next engine was asked");
                let _ = std::fs::remove_dir_all(&d);
            }
        }

        #[test]
        fn an_engine_still_at_it_is_slow_and_not_asked_again_elsewhere() {
            let d = dir("slow");
            let (a, b) = (d.join("a"), d.join("b"));
            let ha = serve(&a, "HTTP/1.0 204 No Content\r\n\r\n", Duration::from_millis(1500));
            let lb = quiet(&b);
            let o = run(&target(Action::Stop), vec![Endpoint::Unix(a), Endpoint::Unix(b)], Duration::from_millis(200));
            assert_eq!(o, Outcome::Slow);
            assert!(ha.join().unwrap().is_some());
            assert!(lb.accept().is_err(), "the next engine was asked");
            let _ = std::fs::remove_dir_all(&d);
        }

        #[test]
        fn one_this_user_may_not_use_outweighs_one_that_does_not_know_it() {
            use std::os::unix::fs::PermissionsExt;
            let d = dir("denied");
            let (a, b) = (d.join("a"), d.join("b"));
            let ha = serve(&a, NOT_FOUND, Duration::ZERO);
            let lb = quiet(&b);
            std::fs::set_permissions(&b, std::fs::Permissions::from_mode(0o000)).unwrap();
            if UnixStream::connect(&b).is_ok() {
                // Root, or a system that does not check a socket's mode.
                drop(ha);
                let _ = std::fs::remove_dir_all(&d);
                return;
            }
            let o = run(&target(Action::Start), vec![Endpoint::Unix(a), Endpoint::Unix(b)], PATIENCE);
            assert_eq!(o, Outcome::Denied);
            assert!(ha.join().unwrap().is_some());
            drop(lb);
            let _ = std::fs::remove_dir_all(&d);
        }
    }
}
