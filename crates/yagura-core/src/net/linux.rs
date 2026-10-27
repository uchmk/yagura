//! Linux: what kind each interface is from `/sys/class/net`, the sockets
//! from `/proc/net/{tcp,tcp6,udp,udp6}` and whose they are from the
//! `socket:[inode]` links in `/proc/*/fd`.

#[cfg(target_os = "linux")]
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
#[cfg(target_os = "linux")]
use std::path::Path;

use super::{Conn, Kind, Proto, State, peer};
#[cfg(target_os = "linux")]
use super::{Conns, Nic, Note};

/// `DEVTYPE=` from a sysfs `uevent` file (`bluetooth`, `wlan`, `bridge`).
pub fn devtype(uevent: &str) -> Option<&str> {
    uevent.lines().find_map(|l| l.strip_prefix("DEVTYPE=")).map(str::trim)
}

/// The kind from sysfs: `type` (ARPHRD_*), the `DEVTYPE` from `uevent`,
/// whether it is wireless, whether a device stands behind it.
pub fn kind(name: &str, ty: u32, devtype: Option<&str>, wireless: bool, has_device: bool) -> Kind {
    match ty {
        772 => Kind::Loopback,
        // bnep: the Bluetooth network encapsulation (PAN) driver.
        _ if devtype == Some("bluetooth") || name.starts_with("bnep") => Kind::Bluetooth,
        _ if wireless => Kind::Wireless,
        // none (tun), PPP, and the IP-in-IP tunnels.
        65534 | 512 | 768 | 769 | 776 | 778 | 823 => Kind::Vpn,
        1 if ["wg", "tun", "tap", "tailscale", "zt"].iter().any(|p| name.starts_with(p)) => Kind::Vpn,
        1 if has_device => Kind::Wired,
        1 => Kind::Virtual,
        _ => Kind::Other,
    }
}

/// Add what sysinfo leaves out: the kind, the link speed and the driver.
#[cfg(target_os = "linux")]
pub fn fill(n: &mut Nic, root: &Path) {
    let dir = root.join(&n.name);
    let read = |f: &str| std::fs::read_to_string(dir.join(f)).ok().map(|s| s.trim().to_string());
    let ty = read("type").and_then(|s| s.parse().ok()).unwrap_or(0);
    let wireless = dir.join("wireless").exists() || dir.join("phy80211").exists();
    let has_device = dir.join("device").exists();
    let uevent = read("uevent").unwrap_or_default();
    n.kind = kind(&n.name, ty, devtype(&uevent), wireless, has_device);
    // Reading it fails while the link is down; -1 when the driver does not know.
    n.speed_mbps = read("speed").and_then(|s| s.parse::<i64>().ok()).filter(|&s| s > 0 && n.up).map(|s| s as u64);
    if let Some(d) =
        std::fs::read_link(dir.join("device/driver")).ok().and_then(|p| p.file_name().map(|f| f.to_string_lossy().into_owned()))
    {
        n.desc = d;
    }
}

/// `0100007F:0016`: the address as the kernel keeps it in memory, printed
/// as 32-bit words in host order, then the port in hex.
fn addr(s: &str) -> Option<SocketAddr> {
    let (ip, port) = s.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    let word = |i: usize| u32::from_str_radix(ip.get(i * 8..i * 8 + 8)?, 16).ok().map(u32::to_ne_bytes);
    let ip = match ip.len() {
        8 => IpAddr::V4(Ipv4Addr::from(word(0)?)),
        32 => {
            let mut b = [0u8; 16];
            for i in 0..4 {
                b[i * 4..i * 4 + 4].copy_from_slice(&word(i)?);
            }
            let v6 = Ipv6Addr::from(b);
            // Show an IPv4 client of a dual-stack socket as plain IPv4.
            v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4)
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

/// The kernel's TCP state numbers (include/net/tcp_states.h).
pub fn tcp_state(st: u8) -> State {
    match st {
        0x01 => State::Established,
        0x02 => State::SynSent,
        0x03 => State::SynRecv,
        0x04 => State::FinWait1,
        0x05 => State::FinWait2,
        0x06 => State::TimeWait,
        0x08 => State::CloseWait,
        0x09 => State::LastAck,
        0x0A => State::Listen,
        0x0B => State::Closing,
        _ => State::Close,
    }
}

/// One `/proc/net` table: each socket with its inode (0 when it has no
/// file, as in TIME_WAIT).
pub fn parse(text: &str, proto: Proto) -> Vec<(Conn, u64)> {
    let mut out = Vec::new();
    for line in text.lines().skip(1) {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 10 {
            continue;
        }
        let (Some(local), Some(remote), Ok(st), Ok(inode)) = (addr(f[1]), addr(f[2]), u8::from_str_radix(f[3], 16), f[9].parse::<u64>())
        else {
            continue;
        };
        let state = if proto.is_udp() { State::None } else { tcp_state(st) };
        out.push((Conn { proto, local, remote: peer(remote), state, pid: None }, inode));
    }
    out
}

/// The inode in an fd link that is a socket: `socket:[12345]`.
pub fn socket_inode(link: &str) -> Option<u64> {
    link.strip_prefix("socket:[")?.strip_suffix(']')?.parse().ok()
}

/// Socket inode to PID, from every process whose fds this user may read.
#[cfg(target_os = "linux")]
fn owners() -> HashMap<u64, u32> {
    let mut out = HashMap::new();
    let Ok(procs) = std::fs::read_dir("/proc") else { return out };
    for p in procs.flatten() {
        let Some(pid) = p.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        let Ok(fds) = std::fs::read_dir(p.path().join("fd")) else { continue };
        for fd in fds.flatten() {
            if let Some(inode) = std::fs::read_link(fd.path()).ok().and_then(|l| socket_inode(&l.to_string_lossy())) {
                out.entry(inode).or_insert(pid);
            }
        }
    }
    out
}

#[cfg(target_os = "linux")]
pub fn conns() -> Conns {
    let mut found = Vec::new();
    let mut read_any = false;
    for (file, proto) in [("tcp", Proto::Tcp), ("tcp6", Proto::Tcp6), ("udp", Proto::Udp), ("udp6", Proto::Udp6)] {
        // tcp6 and udp6 are missing where IPv6 is off.
        if let Ok(text) = std::fs::read_to_string(Path::new("/proc/net").join(file)) {
            read_any = true;
            found.extend(parse(&text, proto));
        }
    }
    if !read_any {
        return Conns { list: Vec::new(), source: "/proc/net", note: Some(Note::Failed) };
    }
    let owners = if found.iter().any(|&(_, i)| i != 0) { owners() } else { HashMap::new() };
    let mut hidden = false;
    let list = found
        .into_iter()
        .map(|(mut c, inode)| {
            if inode != 0 {
                c.pid = owners.get(&inode).copied();
                hidden |= c.pid.is_none();
            }
            c
        })
        .collect();
    Conns { list, source: "/proc/net", note: hidden.then_some(Note::OtherUsers) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sysfs_types_fold_into_kinds() {
        assert_eq!(kind("lo", 772, None, false, false), Kind::Loopback);
        assert_eq!(kind("wlp2s0", 1, None, true, true), Kind::Wireless);
        assert_eq!(kind("enp3s0", 1, None, false, true), Kind::Wired);
        assert_eq!(kind("docker0", 1, None, false, false), Kind::Virtual);
        assert_eq!(kind("tun0", 65534, None, false, false), Kind::Vpn);
        assert_eq!(kind("wg0", 65534, None, false, false), Kind::Vpn);
        assert_eq!(kind("tap0", 1, None, false, false), Kind::Vpn);
        assert_eq!(kind("can0", 280, None, false, true), Kind::Other);
        assert_eq!(kind("bnep0", 1, Some("bluetooth"), false, false), Kind::Bluetooth);
        // An older kernel without DEVTYPE: the name still tells.
        assert_eq!(kind("bnep1", 1, None, false, false), Kind::Bluetooth);
        assert_eq!(kind("br0", 1, Some("bridge"), false, false), Kind::Virtual);
    }

    #[test]
    fn uevent_gives_the_devtype() {
        assert_eq!(devtype("INTERFACE=bnep0\nIFINDEX=7\nDEVTYPE=bluetooth\n"), Some("bluetooth"));
        assert_eq!(devtype("INTERFACE=eth0\nIFINDEX=2\n"), None);
    }

    #[test]
    fn proc_net_tables_parse() {
        let tcp = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
           0: 0100007F:0016 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 1190 1 0 100 0 0 10 0\n\
           1: 0200000A:C350 08080808:01BB 01 00000000:00000000 00:00000000 00000000  1000        0 2889 1 0 20 4 30 10 -1\n\
           2: 0200000A:C351 08080808:01BB 06 00000000:00000000 03:00000F1A 00000000     0        0 0 3 0\n\
           bad line\n";
        let got = parse(tcp, Proto::Tcp);
        assert_eq!(got.len(), 3);
        let (c, inode) = &got[0];
        assert_eq!((c.local, c.remote, c.state, *inode), ("127.0.0.1:22".parse().unwrap(), None, State::Listen, 1190));
        let (c, _) = &got[1];
        assert_eq!(
            (c.local, c.remote, c.state),
            ("10.0.0.2:50000".parse().unwrap(), Some("8.8.8.8:443".parse().unwrap()), State::Established)
        );
        assert_eq!((got[2].0.state, got[2].1), (State::TimeWait, 0));

        let udp6 = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops\n\
           0: 00000000000000000000000000000000:14E9 00000000000000000000000000000000:0000 07 00000000:00000000 00:00000000 00000000   108        0 4321 2 0 0\n\
           1: 0000000000000000FFFF00000100007F:0035 00000000000000000000000001000000:0000 07 00000000:00000000 00:00000000 00000000     0        0 99 2 0 0\n";
        let got = parse(udp6, Proto::Udp6);
        assert_eq!((got[0].0.local, got[0].0.state, got[0].0.remote), ("[::]:5353".parse().unwrap(), State::None, None));
        // An IPv4-mapped address reads as IPv4; ::1 with port 0 is still a peer address.
        assert_eq!(got[1].0.local, "127.0.0.1:53".parse().unwrap());
        assert_eq!(got[1].0.remote, Some("[::1]:0".parse().unwrap()));
    }

    #[test]
    fn socket_links_give_their_inode() {
        assert_eq!(socket_inode("socket:[12345]"), Some(12345));
        assert_eq!(socket_inode("pipe:[12345]"), None);
        assert_eq!(socket_inode("/dev/null"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sysfs_fills_the_kind_speed_and_driver() {
        let root = crate::util::test_dir("net-sysfs");
        let eth = root.join("eth0");
        std::fs::create_dir_all(eth.join("device")).unwrap();
        std::fs::create_dir_all(root.join("drivers/e1000e")).unwrap();
        std::os::unix::fs::symlink(root.join("drivers/e1000e"), eth.join("device/driver")).unwrap();
        std::fs::write(eth.join("type"), "1\n").unwrap();
        std::fs::write(eth.join("speed"), "1000\n").unwrap();
        let mut n = Nic { name: "eth0".into(), up: true, ..Nic::default() };
        fill(&mut n, &root);
        assert_eq!((n.kind, n.speed_mbps, n.desc.as_str()), (Kind::Wired, Some(1000), "e1000e"));
        // Down: no speed, whatever the file says.
        let mut n = Nic { name: "eth0".into(), up: false, ..Nic::default() };
        fill(&mut n, &root);
        assert_eq!(n.speed_mbps, None);
        // Bluetooth PAN: Ethernet in `type`, told apart by uevent.
        let bnep = root.join("pan0");
        std::fs::create_dir_all(&bnep).unwrap();
        std::fs::write(bnep.join("type"), "1\n").unwrap();
        std::fs::write(bnep.join("uevent"), "INTERFACE=pan0\nIFINDEX=7\nDEVTYPE=bluetooth\n").unwrap();
        let mut n = Nic { name: "pan0".into(), up: true, ..Nic::default() };
        fill(&mut n, &root);
        assert_eq!(n.kind, Kind::Bluetooth);
    }
}
