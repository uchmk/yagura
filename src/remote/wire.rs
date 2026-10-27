//! The sampler's messages on the wire: reading back the few `&'static str`
//! fields, keeping process lines short, and which viewer wants which
//! message. Pure, so every OS tests it.

use super::proto::Want;
use crate::collect::{Msg, Proc};

#[cfg(test)]
use yagura_core::wire::{NAME_MAX, intern};

/// A process's command line and path past this many bytes are cut: a few
/// run with whole scripts on theirs.
pub const LINE_MAX: usize = 512;

fn cut(s: &mut String) {
    if s.len() > LINE_MAX {
        let mut at = LINE_MAX;
        while !s.is_char_boundary(at) {
            at -= 1;
        }
        s.truncate(at);
    }
}

pub fn trim_procs(procs: &mut [Proc]) {
    for p in procs {
        cut(&mut p.cmd);
        if let Some(e) = &mut p.exe {
            cut(e);
        }
    }
}

/// Sample processes every this many rounds, so a viewer gets them about
/// every 4 seconds (and at most every other round, as the window does).
pub fn procs_every(interval: f32) -> u32 {
    (4.0 / interval.max(0.1)).ceil().max(2.0) as u32
}

/// Which viewers a message goes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// Every reading the screens draw from: those who watch the machine.
    Detail,
    /// Only those on its svc screen.
    Services,
    /// Only those on its net screen.
    Conns,
    /// Only those on its devices screen.
    Devices,
    /// Only those on its containers screen.
    Containers,
    /// What does not change: everyone who watches, and kept for those who
    /// start watching later.
    Static,
}

pub fn route(m: &Msg) -> Route {
    match m {
        Msg::Static(_) | Msg::Hardware(_) | Msg::Session(_) => Route::Static,
        Msg::Svc(_) => Route::Services,
        Msg::Conns(_) => Route::Conns,
        Msg::Devices(_) => Route::Devices,
        Msg::Containers(_) => Route::Containers,
        _ => Route::Detail,
    }
}

impl Want {
    /// Whether a viewer that wants this gets `r`.
    pub fn takes(self, r: Route) -> bool {
        match r {
            Route::Detail | Route::Static => self.detail,
            Route::Services => self.detail && self.services,
            Route::Conns => self.detail && self.conns,
            Route::Devices => self.detail && self.devices,
            Route::Containers => self.detail && self.containers,
        }
    }
}

/// What the sampler has to read for all the viewers together.
pub fn union(wants: impl IntoIterator<Item = Want>) -> Want {
    wants.into_iter().fold(Want::default(), |a, w| Want {
        detail: a.detail || w.detail,
        services: a.services || (w.detail && w.services),
        conns: a.conns || (w.detail && w.conns),
        devices: a.devices || (w.detail && w.devices),
        containers: a.containers || (w.detail && w.containers),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect::{Mem, Static, battery, containers, devices, disk, gpu, hw, net, sensors, session, svc, topo};
    use crate::remote::proto;

    fn every_msg() -> Vec<Msg> {
        let topo = topo::Topology { groups: Vec::new(), logical: 4, source: "sysfs" };
        let st = Static {
            host: "nas".into(),
            os: "Linux".into(),
            kernel: "6.1".into(),
            arch: "x86_64".into(),
            brand: "CPU".into(),
            boot_time: 7,
            topo,
        };
        let proc = Proc {
            pid: 1,
            ppid: None,
            name: "init".into(),
            cpu: 1.5,
            mem: 4096,
            threads: Some(1),
            user: Some("root".into()),
            cmd: "/sbin/init".into(),
            exe: None,
            start: 3,
            status: "Sleep".into(),
        };
        let dev = gpu::Device {
            id: "drm:card0".into(),
            vendor: Some("AMD"),
            source: "sysfs+fdinfo",
            usage: Some(40.0),
            fan: Some(gpu::Fan::Rpm(1200)),
            why: vec![(gpu::Field::Temp, gpu::Why::NoSensor)],
            ..gpu::Device::default()
        };
        let conn = net::Conn {
            proto: net::Proto::Tcp6,
            local: "[::1]:22".parse().unwrap(),
            remote: Some("127.0.0.1:5000".parse().unwrap()),
            state: net::State::Established,
            pid: Some(9),
        };
        let drive = disk::Drive { id: "sda".into(), bus: Some("sata"), busy: Some(3.0), ..disk::Drive::default() };
        vec![
            Msg::Static(Box::new(st)),
            Msg::Cpu { usage: vec![1.0, 2.0], freq_mhz: vec![3000, 3100], total: 1.5, load: Some((0.1, 0.2, 0.3)) },
            Msg::Mem(Mem { total: 16, used: 4, available: 12, swap_total: 2, swap_used: 1 }),
            Msg::Procs { procs: vec![proc], total_threads: Some(9) },
            Msg::Gpu(vec![dev]),
            Msg::Svc(svc::Services {
                list: vec![svc::Service { name: "sshd".into(), ..svc::Service::default() }],
                source: "systemd",
                note: None,
            }),
            Msg::Nics(vec![net::Nic { name: "eth0".into(), ..net::Nic::default() }]),
            Msg::Disks(disk::Disks { drives: vec![drive], volumes: Vec::new(), note: Some(disk::Note::NoDiskstats) }),
            Msg::Sensors(sensors::Sensors {
                list: vec![sensors::Sensor { id: "hwmon0/temp1".into(), ..sensors::Sensor::default() }],
                note: None,
            }),
            Msg::Conns(net::Conns { list: vec![conn], source: "/proc/net", note: Some(net::Note::OtherUsers) }),
            Msg::Hardware(Box::new(hw::Hardware { source: "SMBIOS 3.3".into(), ..hw::Hardware::default() })),
            Msg::Battery(Some(battery::Battery { pct: 87.0, charging: true, plugged: true })),
            Msg::Devices(Box::new(devices::Devices {
                list: vec![devices::Device {
                    id: r"USB\VID_046D&PID_C52B\5&1".into(),
                    parent: Some("hub".into()),
                    name: "USB Receiver".into(),
                    kind: devices::Kind::Input,
                    status: devices::Status::Problem(43),
                    bus: devices::Bus::Usb { vid: 0x046d, pid: 0xc52b, rev: Some(0x1211), speed_kbps: Some(12_000) },
                    ..devices::Device::default()
                }],
                note: Some(devices::Note::NoIds),
            })),
            Msg::Containers(Box::new(containers::Containers {
                list: vec![containers::Container {
                    id: "b3f1c2d4e5f6".into(),
                    name: "web".into(),
                    state: containers::State::Running,
                    cpu: Some(12.5),
                    mem: Some(1 << 20),
                    ..containers::Container::default()
                }],
                runtime: Some(containers::Runtime::Podman),
                note: None,
                engine: None,
            })),
            Msg::Session(session::Session {
                users: vec![session::User { name: "ann".into(), line: "tty1".into(), host: String::new(), since: 9 }],
                power: "balanced".into(),
            }),
        ]
    }

    #[test]
    fn every_message_goes_round() {
        for m in every_msg() {
            let back: Msg = proto::decode(&proto::frame(&m)[4..]).unwrap();
            assert_eq!(back, m);
        }
    }

    #[test]
    fn names_are_kept_once_and_bounded() {
        let a = intern("wire-test-name");
        assert!(std::ptr::eq(a, intern("wire-test-name")));
        assert_eq!(intern(&"x".repeat(NAME_MAX + 1)), "?");
    }

    #[test]
    fn long_lines_are_cut_on_a_character() {
        let mut p = vec![Proc {
            pid: 1,
            ppid: None,
            name: String::new(),
            cpu: 0.0,
            mem: 0,
            threads: None,
            user: None,
            cmd: format!("a{}", "é".repeat(LINE_MAX)),
            exe: Some("short".into()),
            start: 0,
            status: String::new(),
        }];
        trim_procs(&mut p);
        assert!(p[0].cmd.len() <= LINE_MAX && p[0].cmd.len() >= LINE_MAX - 1, "{}", p[0].cmd.len());
        assert_eq!(p[0].exe.as_deref(), Some("short"));
    }

    #[test]
    fn processes_come_about_every_four_seconds() {
        assert_eq!(procs_every(1.0), 4);
        assert_eq!(procs_every(0.5), 8);
        assert_eq!(procs_every(2.0), 2);
        assert_eq!(procs_every(10.0), 2);
    }

    #[test]
    fn each_message_goes_where_it_is_wanted() {
        let routes: Vec<Route> = every_msg().iter().map(route).collect();
        assert_eq!(routes[0], Route::Static);
        assert_eq!(routes[1], Route::Detail);
        assert_eq!(routes[5], Route::Services);
        assert_eq!(routes[9], Route::Conns);
        assert_eq!(routes[10], Route::Static);
        assert_eq!(routes[12], Route::Devices);
        assert_eq!(routes[13], Route::Containers);
        assert_eq!(routes[14], Route::Static);
        let watch = Want { detail: true, ..Want::default() };
        let svc = Want { services: true, ..watch };
        assert!(watch.takes(Route::Detail) && !watch.takes(Route::Services));
        assert!(svc.takes(Route::Services) && !svc.takes(Route::Conns));
        let dev = Want { devices: true, ..watch };
        assert!(dev.takes(Route::Devices) && !watch.takes(Route::Devices) && !dev.takes(Route::Services));
        let ct = Want { containers: true, ..watch };
        assert!(ct.takes(Route::Containers) && !watch.takes(Route::Containers) && !ct.takes(Route::Devices));
        assert_eq!(union([Want { devices: true, ..Want::default() }]), Want::default());
        assert_eq!(union([watch, dev]), dev);
        assert!(!Want::default().takes(Route::Static));
        // Services without watching the machine read nothing.
        let stray = Want { services: true, ..Want::default() };
        assert_eq!(union([stray]), Want::default());
        assert_eq!(union([watch, svc]), svc);
        assert_eq!(union([]), Want::default());
    }
}
