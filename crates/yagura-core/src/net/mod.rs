//! Network: the interfaces with their traffic (read every tick, so the
//! graphs have a minute behind them when the net screen opens) and the
//! sockets (read only while that screen shows).
//!
//! Linux takes the interfaces from sysinfo and what kind each is from
//! `/sys/class/net`, the sockets from `/proc/net` with their processes from
//! `/proc/*/fd`. Windows asks the IP helper for both. macOS takes the
//! interfaces from sysinfo and their kinds from `networksetup`, the sockets
//! from `netstat`. The parsing is kept in pure functions so the tests run
//! everywhere.

#[cfg(windows)]
mod iphlp;
#[cfg(any(target_os = "linux", test))]
pub mod linux;
#[cfg(any(target_os = "macos", test))]
pub mod mac;
#[cfg(any(windows, test))]
pub mod windows;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::time::Instant;

/// What an interface is, as far as the OS lets on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
// Other OSes read no sockets and guess kinds from names alone.
#[cfg_attr(not(any(windows, target_os = "linux", target_os = "macos")), allow(dead_code))]
pub enum Kind {
    Wired,
    Wireless,
    /// Bluetooth PAN (tethering through a phone); the radio itself is not
    /// a network interface.
    Bluetooth,
    Vpn,
    Virtual,
    #[default]
    Other,
    Loopback,
}

impl Kind {
    pub const ALL: [Kind; 7] = [Self::Wired, Self::Wireless, Self::Bluetooth, Self::Vpn, Self::Virtual, Self::Other, Self::Loopback];

    pub fn key(self) -> &'static str {
        match self {
            Self::Wired => "net.kind.wired",
            Self::Wireless => "net.kind.wireless",
            Self::Bluetooth => "net.kind.bluetooth",
            Self::Vpn => "net.kind.vpn",
            Self::Virtual => "net.kind.virtual",
            Self::Other => "net.kind.other",
            Self::Loopback => "net.kind.loopback",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Nic {
    /// The interface name (`eth0`, `en0`) or Windows' alias (`Wi-Fi`).
    pub name: String,
    /// The adapter's description on Windows, the driver on Linux, the
    /// hardware port on macOS (`Wi-Fi`); empty when there is none.
    pub desc: String,
    pub kind: Kind,
    /// Carrying traffic: the link is up (or the OS cannot say).
    pub up: bool,
    pub speed_mbps: Option<u64>,
    pub mac: Option<String>,
    /// `addr/prefix`.
    pub ipv4: Vec<String>,
    pub ipv6: Vec<String>,
    pub mtu: Option<u64>,
    /// Bytes since the interface came up.
    pub rx_total: u64,
    pub tx_total: u64,
    /// Bytes per second since the last reading; 0 on the first.
    pub rx_rate: f64,
    pub tx_rate: f64,
}

/// `aa:bb:cc:dd:ee:ff`, or `None` for no address or all zeros.
pub fn mac(b: &[u8]) -> Option<String> {
    if b.is_empty() || b.iter().all(|&x| x == 0) {
        return None;
    }
    Some(b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(":"))
}

/// Fill in the rates from the totals of the last reading, `secs` ago.
fn rates(list: &mut [Nic], last: &HashMap<String, (u64, u64)>, secs: Option<f64>) {
    let Some(secs) = secs.filter(|&s| s > 0.0) else { return };
    for n in list {
        if let Some(&(rx, tx)) = last.get(&n.name) {
            // A counter that went back (the interface was reset) reads as 0.
            n.rx_rate = n.rx_total.saturating_sub(rx) as f64 / secs;
            n.tx_rate = n.tx_total.saturating_sub(tx) as f64 / secs;
        }
    }
}

/// Up first, loopback last, then by kind and name.
fn sort(list: &mut [Nic]) {
    list.sort_by(|a, b| {
        (!a.up, a.kind == Kind::Loopback, a.kind, a.name.to_lowercase()).cmp(&(
            !b.up,
            b.kind == Kind::Loopback,
            b.kind,
            b.name.to_lowercase(),
        ))
    });
}

/// Reads the interfaces and works out their rates.
pub struct NicReader {
    #[cfg(not(windows))]
    nets: sysinfo::Networks,
    #[cfg(target_os = "macos")]
    ports: mac::Ports,
    last: HashMap<String, (u64, u64)>,
    at: Option<Instant>,
}

impl NicReader {
    pub fn new() -> Self {
        Self {
            #[cfg(not(windows))]
            nets: sysinfo::Networks::new_with_refreshed_list(),
            #[cfg(target_os = "macos")]
            ports: mac::Ports::default(),
            last: HashMap::new(),
            at: None,
        }
    }

    pub fn read(&mut self) -> Vec<Nic> {
        let mut list = self.read_os();
        let now = Instant::now();
        rates(&mut list, &self.last, self.at.map(|t| now.duration_since(t).as_secs_f64()));
        self.last = list.iter().map(|n| (n.name.clone(), (n.rx_total, n.tx_total))).collect();
        self.at = Some(now);
        sort(&mut list);
        list
    }

    #[cfg(windows)]
    fn read_os(&mut self) -> Vec<Nic> {
        iphlp::nics()
    }

    #[cfg(not(windows))]
    fn read_os(&mut self) -> Vec<Nic> {
        self.nets.refresh(true);
        #[cfg(target_os = "macos")]
        let ports = self.ports.get();
        self.nets
            .list()
            .iter()
            .map(|(name, d)| {
                let mut n = from_sysinfo(name, d);
                #[cfg(target_os = "linux")]
                linux::fill(&mut n, std::path::Path::new("/sys/class/net"));
                #[cfg(target_os = "macos")]
                match ports.get(name.as_str()) {
                    Some(port) => {
                        n.kind = mac::port_kind(port);
                        n.desc = port.clone();
                    }
                    None => n.kind = guess_kind(name),
                }
                #[cfg(not(any(target_os = "linux", target_os = "macos")))]
                {
                    n.kind = guess_kind(name);
                }
                n
            })
            .collect()
    }
}

#[cfg(not(windows))]
fn from_sysinfo(name: &str, d: &sysinfo::NetworkData) -> Nic {
    use sysinfo::InterfaceOperationalState as Op;
    let (mut ipv4, mut ipv6) = (Vec::new(), Vec::new());
    for ip in d.ip_networks() {
        let s = format!("{}/{}", ip.addr, ip.prefix);
        if ip.addr.is_ipv4() { ipv4.push(s) } else { ipv6.push(s) }
    }
    ipv4.sort();
    ipv6.sort();
    Nic {
        name: name.to_string(),
        up: matches!(d.operational_state(), Op::Up | Op::Unknown),
        mac: mac(&d.mac_address().0),
        ipv4,
        ipv6,
        mtu: Some(d.mtu()).filter(|&m| m > 0),
        rx_total: d.total_received(),
        tx_total: d.total_transmitted(),
        ..Nic::default()
    }
}

/// The kind from the name alone, BSD style: for what macOS has no hardware
/// port for (VPNs, AirDrop, VM bridges).
#[cfg(any(not(any(windows, target_os = "linux")), test))]
pub fn guess_kind(name: &str) -> Kind {
    let starts = |ps: &[&str]| ps.iter().any(|p| name.starts_with(p));
    if name.starts_with("lo") {
        Kind::Loopback
    } else if starts(&["utun", "ipsec", "ppp", "tun", "tap", "wg"]) {
        Kind::Vpn
    } else if starts(&["bridge", "awdl", "llw", "anpi", "ap", "gif", "stf", "vmenet", "vnic"]) {
        Kind::Virtual
    } else {
        Kind::Other
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(not(any(windows, target_os = "linux", target_os = "macos")), allow(dead_code))]
pub enum Proto {
    Tcp,
    Tcp6,
    Udp,
    Udp6,
}

impl Proto {
    pub fn label(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Tcp6 => "tcp6",
            Self::Udp => "udp",
            Self::Udp6 => "udp6",
        }
    }

    pub fn is_udp(self) -> bool {
        matches!(self, Self::Udp | Self::Udp6)
    }
}

/// A TCP socket's state; UDP has none.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(not(any(windows, target_os = "linux", target_os = "macos")), allow(dead_code))]
pub enum State {
    Listen,
    Established,
    SynSent,
    SynRecv,
    FinWait1,
    FinWait2,
    TimeWait,
    Close,
    CloseWait,
    LastAck,
    Closing,
    /// UDP.
    None,
}

impl State {
    pub const ALL: [State; 12] = [
        Self::Listen,
        Self::Established,
        Self::SynSent,
        Self::SynRecv,
        Self::FinWait1,
        Self::FinWait2,
        Self::TimeWait,
        Self::Close,
        Self::CloseWait,
        Self::LastAck,
        Self::Closing,
        Self::None,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Self::Listen => "net.state.listen",
            Self::Established => "net.state.established",
            Self::SynSent => "net.state.syn_sent",
            Self::SynRecv => "net.state.syn_recv",
            Self::FinWait1 => "net.state.fin_wait1",
            Self::FinWait2 => "net.state.fin_wait2",
            Self::TimeWait => "net.state.time_wait",
            Self::Close => "net.state.close",
            Self::CloseWait => "net.state.close_wait",
            Self::LastAck => "net.state.last_ack",
            Self::Closing => "net.state.closing",
            Self::None => "net.state.none",
        }
    }

    /// The list's order: listening, UDP, established, then the rest.
    fn rank(self) -> u8 {
        match self {
            Self::Listen => 0,
            Self::None => 1,
            Self::Established => 2,
            Self::SynSent | Self::SynRecv => 3,
            _ => 4,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Conn {
    pub proto: Proto,
    pub local: SocketAddr,
    /// `None` for a socket that is not connected (listening, most UDP).
    pub remote: Option<SocketAddr>,
    pub state: State,
    /// `None` when the OS does not say (another user's socket, TIME_WAIT).
    pub pid: Option<u32>,
}

impl Conn {
    /// Waiting for others: TCP listening or UDP bound to nothing in particular.
    pub fn listening(&self) -> bool {
        self.state == State::Listen || (self.proto.is_udp() && self.remote.is_none())
    }
}

/// `None` for the all-zeros address and port: no peer.
#[cfg_attr(not(any(windows, target_os = "linux", target_os = "macos")), allow(dead_code))]
pub fn peer(a: SocketAddr) -> Option<SocketAddr> {
    (!(a.ip().is_unspecified() && a.port() == 0)).then_some(a)
}

/// Whether `c` is on the interface `n`: bound to one of its addresses, or
/// to any address (a listener on `0.0.0.0` takes that interface's traffic
/// too). With no interface, only the latter.
pub fn on_nic(c: &Conn, n: Option<&Nic>) -> bool {
    let ip = c.local.ip().to_canonical();
    let addr = |a: &String| a.split(['/', '%']).next().and_then(|s| s.parse::<IpAddr>().ok());
    ip.is_unspecified() || n.is_some_and(|n| n.ipv4.iter().chain(&n.ipv6).any(|a| addr(a) == Some(ip)))
}

/// Why the list of sockets is short or empty.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[allow(dead_code)]
pub enum Note {
    /// Some sockets' processes are another user's: their PIDs need root.
    OtherUsers,
    /// The OS would not give the table.
    Failed,
    OtherOs,
}

impl Note {
    pub const ALL: [Note; 3] = [Self::OtherUsers, Self::Failed, Self::OtherOs];

    pub fn key(self) -> &'static str {
        match self {
            Self::OtherUsers => "net.note.other_users",
            Self::Failed => "net.note.failed",
            Self::OtherOs => "net.note.other_os",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Conns {
    pub list: Vec<Conn>,
    /// `/proc/net`, `iphlpapi`, `netstat`.
    #[serde(deserialize_with = "crate::wire::name")]
    pub source: crate::Name,
    pub note: Option<Note>,
}

/// Listening first, then UDP, then established; by local port within.
fn sort_conns(list: &mut [Conn]) {
    list.sort_by_key(|c| (c.state.rank(), c.local.port(), c.local.ip(), c.remote.map(|r| (r.ip(), r.port()))));
}

/// Every socket this user may see.
pub fn read_conns() -> Conns {
    let mut out = read_conns_os();
    sort_conns(&mut out.list);
    out
}

#[cfg(target_os = "linux")]
fn read_conns_os() -> Conns {
    linux::conns()
}

#[cfg(windows)]
fn read_conns_os() -> Conns {
    iphlp::conns()
}

#[cfg(target_os = "macos")]
fn read_conns_os() -> Conns {
    mac::conns()
}

#[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
fn read_conns_os() -> Conns {
    Conns { list: Vec::new(), source: "", note: Some(Note::OtherOs) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::tr;

    #[test]
    fn every_kind_state_and_note_has_english() {
        for k in Kind::ALL {
            assert_ne!(tr(k.key()), k.key(), "{k:?}");
        }
        for s in State::ALL {
            assert_ne!(tr(s.key()), s.key(), "{s:?}");
        }
        for n in Note::ALL {
            assert_ne!(tr(n.key()), n.key(), "{n:?}");
        }
    }

    #[test]
    fn a_socket_is_on_an_interface_by_its_local_address() {
        let n = Nic { ipv4: vec!["192.168.1.10/24".into()], ipv6: vec!["fe80::1/64".into()], ..Nic::default() };
        let c = |local: &str| Conn { proto: Proto::Tcp, local: local.parse().unwrap(), remote: None, state: State::Listen, pid: None };
        assert!(on_nic(&c("192.168.1.10:22"), Some(&n)));
        assert!(on_nic(&c("[fe80::1]:123"), Some(&n)));
        // An IPv4 address seen through an IPv6 socket is still the interface's.
        assert!(on_nic(&c("[::ffff:192.168.1.10]:80"), Some(&n)));
        assert!(!on_nic(&c("10.0.0.2:22"), Some(&n)));
        // Bound to any address: on every interface, even one gone from the list.
        assert!(on_nic(&c("0.0.0.0:22"), Some(&n)));
        assert!(on_nic(&c("[::]:22"), None));
        assert!(!on_nic(&c("192.168.1.10:22"), None));
    }

    fn nic(name: &str, kind: Kind, up: bool, rx: u64, tx: u64) -> Nic {
        Nic { name: name.into(), kind, up, rx_total: rx, tx_total: tx, ..Nic::default() }
    }

    #[test]
    fn rates_come_from_the_last_totals() {
        let mut list = vec![nic("eth0", Kind::Wired, true, 3000, 500), nic("new", Kind::Wired, true, 9, 9)];
        rates(&mut list, &HashMap::new(), None);
        assert_eq!(list[0].rx_rate, 0.0);
        let last = HashMap::from([("eth0".to_string(), (1000, 1000))]);
        rates(&mut list, &last, Some(2.0));
        // Down from 1000 to 500: reset, so 0.
        assert_eq!((list[0].rx_rate, list[0].tx_rate), (1000.0, 0.0));
        assert_eq!(list[1].rx_rate, 0.0);
    }

    #[test]
    fn up_interfaces_come_first_and_loopback_last() {
        let mut list = vec![
            nic("lo", Kind::Loopback, true, 0, 0),
            nic("wlan0", Kind::Wireless, false, 0, 0),
            nic("docker0", Kind::Virtual, true, 0, 0),
            nic("eth0", Kind::Wired, true, 0, 0),
        ];
        sort(&mut list);
        let names: Vec<_> = list.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["eth0", "docker0", "lo", "wlan0"]);
    }

    #[test]
    fn macs_and_peers_read_as_nothing_when_all_zeros() {
        assert_eq!(mac(&[0xaa, 0, 1, 2, 3, 0xff]).as_deref(), Some("aa:00:01:02:03:ff"));
        assert_eq!(mac(&[0; 6]), None);
        assert_eq!(peer("0.0.0.0:0".parse().unwrap()), None);
        assert_eq!(peer("[::]:0".parse().unwrap()), None);
        assert!(peer("1.2.3.4:443".parse().unwrap()).is_some());
    }

    #[test]
    fn mac_names_guess_their_kind() {
        assert_eq!(guess_kind("lo0"), Kind::Loopback);
        assert_eq!(guess_kind("utun3"), Kind::Vpn);
        assert_eq!(guess_kind("awdl0"), Kind::Virtual);
        assert_eq!(guess_kind("en0"), Kind::Other);
    }

    #[test]
    fn listening_sockets_sort_first() {
        let c = |proto, local: &str, remote: Option<&str>, state| Conn {
            proto,
            local: local.parse().unwrap(),
            remote: remote.map(|r| r.parse().unwrap()),
            state,
            pid: None,
        };
        let mut list = vec![
            c(Proto::Tcp, "10.0.0.2:51000", Some("1.1.1.1:443"), State::TimeWait),
            c(Proto::Tcp, "10.0.0.2:50000", Some("1.1.1.1:443"), State::Established),
            c(Proto::Udp, "0.0.0.0:53", None, State::None),
            c(Proto::Tcp, "0.0.0.0:22", None, State::Listen),
        ];
        sort_conns(&mut list);
        let ports: Vec<_> = list.iter().map(|c| c.local.port()).collect();
        assert_eq!(ports, [22, 53, 50000, 51000]);
        assert!(list[0].listening() && list[1].listening() && !list[2].listening());
    }
}
