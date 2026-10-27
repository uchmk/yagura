//! macOS: the sockets from `netstat -anvW` (every user's, with the PID of
//! the process that last used each), and what kind each interface is from
//! `networksetup -listallhardwareports` (Wi-Fi, Ethernet, Bluetooth PAN,
//! Thunderbolt Bridge…). The parsing is kept in pure functions so the tests
//! run everywhere.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use super::{Conn, Kind, Proto, State, peer};

/// `192.168.1.5.443`, `fe80::1%lo0.123`, `*.22`, `*.*`: the port after the
/// last dot, `*` for any. A scope (`%lo0`) is dropped.
pub fn addr(s: &str, v6: bool) -> Option<SocketAddr> {
    let (host, port) = s.rsplit_once('.')?;
    let port = if port == "*" { 0 } else { port.parse().ok()? };
    let ip = match host {
        "*" if v6 => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        "*" => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        h => h.split('%').next()?.parse().ok()?,
    };
    Some(SocketAddr::new(ip, port))
}

fn tcp_state(s: &str) -> State {
    match s {
        "LISTEN" => State::Listen,
        "ESTABLISHED" => State::Established,
        "SYN_SENT" => State::SynSent,
        "SYN_RCVD" => State::SynRecv,
        "FIN_WAIT_1" => State::FinWait1,
        "FIN_WAIT_2" => State::FinWait2,
        "TIME_WAIT" => State::TimeWait,
        "CLOSE_WAIT" => State::CloseWait,
        "LAST_ACK" => State::LastAck,
        "CLOSING" => State::Closing,
        _ => State::Close,
    }
}

/// Where the PID is in a row of the header `head`, counted from the left:
/// the columns before it are all one word (only the process name, in
/// newer `process:pid` columns, may have spaces). "Local Address" and
/// "Foreign Address" are two words in the header and one in a row, and a
/// UDP row has no state, so the caller takes one off for those.
fn pid_column(head: &str) -> Option<usize> {
    let words: Vec<&str> = head.split_whitespace().collect();
    let at = words.iter().position(|&w| w == "pid" || w == "process:pid")?;
    let two_word = words[..at].iter().filter(|&&w| w == "Address").count();
    Some(at - two_word)
}

/// The PID from `f[at..]`: a bare number, or the first `name:pid` (a name
/// may have spaces, so it can start a few words before). 0 is no process.
fn pid_at(f: &[&str], at: usize) -> Option<u32> {
    let first = f.get(at)?;
    let pid = match first.parse::<u32>() {
        Ok(p) => p,
        Err(_) => f[at..].iter().find_map(|w| w.rsplit_once(':').and_then(|(_, p)| p.parse().ok()))?,
    };
    (pid != 0).then_some(pid)
}

/// The internet section of `netstat -anvW`: TCP and UDP over IPv4 and IPv6.
/// A `tcp46` socket is an IPv6 one that takes IPv4 too.
pub fn netstat(text: &str) -> Vec<Conn> {
    let mut out = Vec::new();
    let mut inet = false;
    let mut pid_col = None;
    for line in text.lines() {
        if line.starts_with("Active ") {
            inet = line.starts_with("Active Internet connections");
            continue;
        }
        if !inet {
            continue;
        }
        if line.starts_with("Proto") {
            pid_col = pid_column(line);
            continue;
        }
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 5 {
            continue;
        }
        let proto = match f[0] {
            "tcp4" => Proto::Tcp,
            "tcp6" | "tcp46" => Proto::Tcp6,
            "udp4" => Proto::Udp,
            "udp6" | "udp46" => Proto::Udp6,
            _ => continue,
        };
        let v6 = matches!(proto, Proto::Tcp6 | Proto::Udp6);
        let (Some(local), Some(remote)) = (addr(f[3], v6), addr(f[4], v6)) else { continue };
        let state = if proto.is_udp() {
            State::None
        } else {
            match f.get(5) {
                Some(s) => tcp_state(s),
                None => continue,
            }
        };
        let pid = pid_col.and_then(|at| pid_at(&f, if proto.is_udp() { at.saturating_sub(1) } else { at }));
        out.push(Conn { proto, local, remote: peer(remote), state, pid });
    }
    out
}

/// Whether some sockets that belong to a process show none: what the note
/// says without root. TIME_WAIT and closed ones belong to no process.
pub fn hidden(list: &[Conn]) -> bool {
    list.iter().any(|c| c.pid.is_none() && !matches!(c.state, State::TimeWait | State::Close))
}

/// Device to hardware port, from `networksetup -listallhardwareports`:
/// `Hardware Port: Wi-Fi` then `Device: en0`, a blank line between ports.
pub fn ports(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut port = None;
    for line in text.lines() {
        if let Some(p) = line.strip_prefix("Hardware Port:") {
            port = Some(p.trim().to_string());
        } else if let Some(d) = line.strip_prefix("Device:")
            && let Some(p) = port.take()
        {
            out.insert(d.trim().to_string(), p);
        }
    }
    out
}

/// The kind from the hardware port's name.
pub fn port_kind(port: &str) -> Kind {
    let p = port.to_lowercase();
    let has = |ws: &[&str]| ws.iter().any(|w| p.contains(w));
    if has(&["wi-fi", "airport", "wlan"]) {
        Kind::Wireless
    } else if p.contains("bluetooth") {
        Kind::Bluetooth
    } else if p.contains("bridge") {
        Kind::Virtual
    } else if p.contains("vpn") {
        Kind::Vpn
    } else if has(&["ethernet", "lan", "thunderbolt", "usb", "iphone"]) {
        Kind::Wired
    } else {
        Kind::Other
    }
}

#[cfg(target_os = "macos")]
fn run(prog: &str, args: &[&str]) -> Option<String> {
    use std::process::{Command, Stdio};
    let out = Command::new(prog).args(args).env("LC_ALL", "C").stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(target_os = "macos")]
pub fn conns() -> super::Conns {
    use super::{Conns, Note};
    let Some(text) = run("/usr/sbin/netstat", &["-anvW"]) else {
        return Conns { list: Vec::new(), source: "netstat", note: Some(Note::Failed) };
    };
    let list = netstat(&text);
    let note = (hidden(&list) && !crate::svc::mac::root()).then_some(Note::OtherUsers);
    Conns { list, source: "netstat", note }
}

/// The hardware ports, read again once a minute: they change only when an
/// adapter is plugged in or set up.
#[cfg(target_os = "macos")]
#[derive(Default)]
pub struct Ports {
    map: HashMap<String, String>,
    at: Option<std::time::Instant>,
}

#[cfg(target_os = "macos")]
impl Ports {
    pub fn get(&mut self) -> &HashMap<String, String> {
        if self.at.is_none_or(|t| t.elapsed().as_secs() >= 60) {
            self.at = Some(std::time::Instant::now());
            if let Some(text) = run("/usr/sbin/networksetup", &["-listallhardwareports"]) {
                self.map = ports(&text);
            }
        }
        &self.map
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Sonoma's `netstat -anvW`, cut down: the pid column is `process:pid`
    // and a UDP row has no state.
    const NEW: &str = "Active Internet connections (including servers)
Proto Recv-Q Send-Q  Local Address          Foreign Address        (state)      rxbytes      txbytes  rhiwat  shiwat          process:pid  state  options           gencnt    flags   flags1 usscnt rtncnt fltrs
tcp4       0      0  192.168.1.10.52345     17.248.1.1.443         ESTABLISHED     12345         6789  131072  131768  Google Chrome H:812  00102 00000008 0000000000a1b2c3 00000080 01000900      0      0 000001
tcp46      0      0  *.22                   *.*                    LISTEN              0            0  131072  131072        sshd-session:1  00180 00000006 0000000000000123 00000000 00000800      0      0 000001
tcp4       0      0  192.168.1.10.52000     17.248.1.1.443         TIME_WAIT           0            0  131072  131768          kernel_task:0  00000 00000000 0000000000000124 00000000 00000000      0      0 000000
udp4       0      0  *.5353                 *.*                                     0            0  786896    9216     mDNSResponder:412  00000 00000000 0000000000000125 00000000 00000000      0      0 000000
udp6       0      0  fe80::1%lo0.123        *.*                                     0            0  786896    9216              ntpd:99  00000 00000000 0000000000000126 00000000 00000000      0      0 000000
icm6       0      0  *.*                    *.*                                     0            0    8192    8192              ping6:7  00000 00000000 0000000000000127 00000000 00000000      0      0 000000
Active Multipath Internet connections
Proto/ID  Flags      Local Address     Foreign Address
tcp4/1   0x0         10.0.0.1.1        10.0.0.2.2
Active LOCAL (UNIX) domain sockets
Address          Type   Recv-Q Send-Q            Inode             Conn             Refs          Nextref Addr
";

    // Monterey's: a bare pid column, then epid.
    const OLD: &str = "Active Internet connections (including servers)
Proto Recv-Q Send-Q  Local Address          Foreign Address        (state)     rhiwat shiwat    pid   epid  state    options
tcp6       0      0  ::1.631                *.*                    LISTEN      131072 131072    210      0 0x0080 0x00000006
udp4       0      0  10.0.0.5.68            *.*                                 196724   9216     55      0 0x0000 0x00000000
";

    #[test]
    fn netstat_rows_give_sockets_and_their_processes() {
        let list = netstat(NEW);
        assert_eq!(list.len(), 5, "{list:?}");
        let c = &list[0];
        assert_eq!((c.proto, c.state, c.pid), (Proto::Tcp, State::Established, Some(812)));
        assert_eq!((c.local.to_string(), c.remote.map(|r| r.to_string())), ("192.168.1.10:52345".into(), Some("17.248.1.1:443".into())));
        assert_eq!((list[1].proto, list[1].local.to_string(), list[1].remote, list[1].pid), (Proto::Tcp6, "[::]:22".into(), None, Some(1)));
        assert_eq!((list[2].state, list[2].pid), (State::TimeWait, None));
        assert_eq!(
            (list[3].proto, list[3].state, list[3].local.to_string(), list[3].pid),
            (Proto::Udp, State::None, "0.0.0.0:5353".into(), Some(412))
        );
        assert_eq!((list[4].local.to_string(), list[4].pid), ("[fe80::1]:123".into(), Some(99)));
        assert!(!hidden(&list));

        let old = netstat(OLD);
        assert_eq!(
            old.iter().map(|c| (c.local.to_string(), c.pid)).collect::<Vec<_>>(),
            [("[::1]:631".into(), Some(210)), ("10.0.0.5:68".into(), Some(55))]
        );
        // No pid column at all: the sockets still show, with no process.
        let bare =
            netstat("Active Internet connections\nProto Recv-Q Send-Q Local Address Foreign Address (state)\ntcp4 0 0 *.80 *.* LISTEN\n");
        assert_eq!((bare.len(), bare[0].pid, hidden(&bare)), (1, None, true));
    }

    #[test]
    fn addresses_read_bsd_style() {
        assert_eq!(addr("*.*", false), Some("0.0.0.0:0".parse().unwrap()));
        assert_eq!(addr("*.443", true), Some("[::]:443".parse().unwrap()));
        assert_eq!(addr("::ffff:1.2.3.4.80", true), Some("[::ffff:1.2.3.4]:80".parse().unwrap()));
        assert_eq!(addr("nonsense", false), None);
    }

    #[test]
    fn hardware_ports_give_kinds() {
        let text = "\nHardware Port: Ethernet\nDevice: en0\nEthernet Address: aa:bb:cc:dd:ee:ff\n\nHardware Port: Wi-Fi\nDevice: en1\nEthernet Address: aa:bb:cc:dd:ee:00\n\nHardware Port: Bluetooth PAN\nDevice: en5\nEthernet Address: N/A\n\nHardware Port: Thunderbolt Bridge\nDevice: bridge0\nEthernet Address: N/A\n\nVLAN Configurations\n===================\n";
        let p = ports(text);
        assert_eq!(p.len(), 4);
        let kind = |d: &str| port_kind(&p[d]);
        assert_eq!([kind("en0"), kind("en1"), kind("en5"), kind("bridge0")], [Kind::Wired, Kind::Wireless, Kind::Bluetooth, Kind::Virtual]);
        assert_eq!(port_kind("USB 10/100/1000 LAN"), Kind::Wired);
        assert_eq!(port_kind("AirPort"), Kind::Wireless);
        assert_eq!(port_kind("Something new"), Kind::Other);
    }
}
