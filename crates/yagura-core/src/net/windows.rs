//! Windows: which rows of the IP helper's interface table are the adapters
//! a person thinks of, and the numbers in its socket tables. Pure, so the
//! tests run everywhere; `iphlp.rs` makes the calls.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use super::{Conn, Kind, Nic, Proto, State, mac, peer};

/// The fields of one `MIB_IF_ROW2` this screen uses.
#[derive(Clone, Debug, Default)]
pub struct Row {
    pub index: u32,
    pub guid: u128,
    pub alias: String,
    pub desc: String,
    pub mac: Vec<u8>,
    pub mtu: u32,
    /// IANA ifType.
    pub ty: u32,
    /// NDIS_PHYSICAL_MEDIUM: Bluetooth PAN says Ethernet in `ty` and only
    /// tells here.
    pub medium: u32,
    pub hardware: bool,
    /// A filter driver's view of another interface: the same traffic again.
    pub filter: bool,
    pub connector: bool,
    pub oper_up: bool,
    /// IfOperStatusNotPresent: a removed device's leftover (Wi-Fi 2, 3…).
    pub gone: bool,
    /// One of a card's extra interfaces that Windows keeps out of its own
    /// lists: NCF_HIDDEN in its class key, or left out by `Get-NetAdapter`.
    pub hidden: bool,
    /// Bits per second.
    pub speed: u64,
    pub rx: u64,
    pub tx: u64,
}

const ETHERNET: u32 = 6;
const LOOPBACK: u32 = 24;
const WIFI: u32 = 71;
const PPP: u32 = 23;
const PROP_VIRTUAL: u32 = 53;
const TUNNEL: u32 = 131;
/// NdisPhysicalMediumBluetooth.
const MEDIUM_BLUETOOTH: u32 = 10;
/// The description of the adapters behind Wi-Fi Direct (Miracast, mobile
/// hotspot); `Get-NetAdapter` hides them.
const WIFI_DIRECT: &str = "Microsoft Wi-Fi Direct Virtual Adapter";

/// NCF_HIDDEN in an adapter's `Characteristics`.
const NCF_HIDDEN: u32 = 0x8;

/// A GUID as the registry writes it (`{6B29FC40-CA47-1067-B31D-00DD010662DA}`,
/// braces optional, either case), in the form of `GUID::to_u128`.
pub fn parse_guid(s: &str) -> Option<u128> {
    let s = s.trim();
    let s = s.strip_prefix('{').and_then(|s| s.strip_suffix('}')).unwrap_or(s);
    let hex: String = s.chars().filter(|&c| c != '-').collect();
    if hex.len() != 32 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    u128::from_str_radix(&hex, 16).ok()
}

/// The GUIDs of the hidden adapters, from each class key's
/// `NetCfgInstanceId` and `Characteristics`.
pub fn hidden(entries: impl IntoIterator<Item = (String, u32)>) -> HashSet<u128> {
    entries.into_iter().filter(|(_, c)| c & NCF_HIDDEN != 0).filter_map(|(id, _)| parse_guid(&id)).collect()
}

/// The hardware interfaces `Get-NetAdapter` leaves out, given the GUIDs
/// it lists (`MSFT_NetAdapter`'s default enumeration skips the hidden
/// ones). The extra ports of a Wi-Fi card (Wi-Fi 3, Wi-Fi 5) have no class
/// key of their own, so this is the only place that tells. Nothing when it
/// listed nothing: a failed answer must not hide every card.
pub fn unlisted(rows: &[Row], listed: &HashSet<u128>) -> HashSet<u128> {
    if listed.is_empty() {
        return HashSet::new();
    }
    rows.iter().filter(|r| r.hardware && !r.gone && !listed.contains(&r.guid)).map(|r| r.guid).collect()
}

fn kind(r: &Row) -> Kind {
    match r.ty {
        _ if r.medium == MEDIUM_BLUETOOTH => Kind::Bluetooth,
        WIFI => Kind::Wireless,
        ETHERNET if r.hardware => Kind::Wired,
        ETHERNET => Kind::Virtual,
        PPP | PROP_VIRTUAL | TUNNEL => Kind::Vpn,
        _ => Kind::Other,
    }
}

/// The adapters worth a row: a physical one with its connector (even when
/// unplugged, so a dead cable shows), Bluetooth PAN (which has no connector
/// flag), or any that is up with an address (VPNs, Hyper-V switches). Each
/// GUID once, without the filter layers, the loopback pseudo-interface
/// (which counts nothing), leftovers of removed devices, the hidden
/// adapters, and the Wi-Fi Direct adapters Windows hides while they are down.
pub fn pick(rows: &[Row], ips: &HashMap<u32, (Vec<String>, Vec<String>)>) -> Vec<Nic> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for r in rows {
        if r.filter || r.ty == LOOPBACK || r.gone || r.hidden || (!r.oper_up && r.desc.starts_with(WIFI_DIRECT)) {
            continue;
        }
        let (ipv4, ipv6) = ips.get(&r.index).cloned().unwrap_or_default();
        let keep = (r.hardware && r.connector) || r.medium == MEDIUM_BLUETOOTH || (r.oper_up && !(ipv4.is_empty() && ipv6.is_empty()));
        if !keep || !seen.insert(r.guid) {
            continue;
        }
        let speed = Some(r.speed).filter(|&s| s != 0 && s != u64::MAX && r.oper_up).map(|s| s / 1_000_000);
        out.push(Nic {
            name: if r.alias.is_empty() { r.desc.clone() } else { r.alias.clone() },
            desc: r.desc.clone(),
            kind: kind(r),
            up: r.oper_up,
            speed_mbps: speed,
            mac: mac(&r.mac),
            ipv4,
            ipv6,
            mtu: Some(r.mtu).filter(|&m| m != 0 && m != u32::MAX).map(u64::from),
            rx_total: r.rx,
            tx_total: r.tx,
            ..Nic::default()
        });
    }
    out
}

/// `MIB_TCP_STATE`.
pub fn tcp_state(st: u32) -> State {
    match st {
        2 => State::Listen,
        3 => State::SynSent,
        4 => State::SynRecv,
        5 => State::Established,
        6 => State::FinWait1,
        7 => State::FinWait2,
        8 => State::CloseWait,
        9 => State::Closing,
        10 => State::LastAck,
        11 => State::TimeWait,
        // CLOSED, DELETE_TCB.
        _ => State::Close,
    }
}

/// The port as the tables keep it: network order in the low 16 bits.
pub fn port(raw: u32) -> u16 {
    u16::from_be(raw as u16)
}

/// An IPv4 address as the tables keep it: network order in memory.
pub fn v4(raw: u32) -> IpAddr {
    IpAddr::V4(Ipv4Addr::from(raw.to_ne_bytes()))
}

pub fn v6(b: [u8; 16]) -> IpAddr {
    let a = Ipv6Addr::from(b);
    a.to_ipv4_mapped().map_or(IpAddr::V6(a), IpAddr::V4)
}

/// One socket; `remote` is `None` for UDP, whose tables have no peer.
pub fn conn(proto: Proto, local: (IpAddr, u32), remote: Option<(IpAddr, u32)>, state: Option<u32>, pid: u32) -> Conn {
    Conn {
        proto,
        local: SocketAddr::new(local.0, port(local.1)),
        remote: remote.and_then(|(ip, p)| peer(SocketAddr::new(ip, port(p)))),
        state: state.map_or(State::None, tcp_state),
        // The idle process: TIME_WAIT and other sockets no one holds.
        pid: (pid != 0).then_some(pid),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(index: u32, guid: u128, alias: &str, ty: u32) -> Row {
        Row { index, guid, alias: alias.into(), desc: format!("{alias} adapter"), ty, mtu: 1500, ..Row::default() }
    }

    #[test]
    fn only_the_adapters_people_mean_are_picked() {
        let eth = Row {
            hardware: true,
            connector: true,
            oper_up: true,
            speed: 1_000_000_000,
            mac: vec![1, 2, 3, 4, 5, 6],
            ..row(4, 1, "Ethernet", 6)
        };
        // Unplugged Wi-Fi: still listed, no speed.
        let wifi = Row { hardware: true, connector: true, speed: 300_000_000, ..row(9, 2, "Wi-Fi", 71) };
        // The same adapter seen through a filter driver.
        let filter = Row { filter: true, ..eth.clone() };
        // A second row with the same GUID.
        let dup = Row { index: 40, ..eth.clone() };
        let lo = Row { oper_up: true, ..row(1, 3, "Loopback Pseudo-Interface 1", 24) };
        let vpn = Row { oper_up: true, speed: u64::MAX, mtu: u32::MAX, ..row(12, 4, "WireGuard", 53) };
        // Up but with no address: a WAN miniport.
        let wan = Row { oper_up: true, ..row(13, 5, "WAN Miniport", 23) };
        let vswitch = Row { oper_up: true, ..row(14, 6, "vEthernet (WSL)", 6) };
        // Bluetooth PAN: ifType Ethernet on hardware but no connector flag, listed even when no phone is paired in.
        let bt = Row { hardware: true, medium: 10, ..row(15, 7, "Bluetooth Network Connection", 6) };
        // A Wi-Fi adapter that was removed, and the hidden Wi-Fi Direct one.
        let stale = Row { hardware: true, connector: true, gone: true, ..row(16, 8, "Wi-Fi 2", 71) };
        let direct =
            Row { hardware: true, connector: true, desc: format!("{WIFI_DIRECT} #2"), ..row(17, 9, "Local Area Connection* 1", 71) };
        // Another interface of the Wi-Fi card, marked hidden in its class key.
        let extra = Row { hardware: true, connector: true, hidden: true, ..row(18, 10, "Wi-Fi 3", 71) };
        let ips = HashMap::from([
            (4, (vec!["192.168.1.2/24".to_string()], vec![])),
            (1, (vec!["127.0.0.1/8".to_string()], vec![])),
            (12, (vec!["10.6.0.2/32".to_string()], vec![])),
            (14, (vec![], vec!["fe80::1/64".to_string()])),
        ]);
        let got = pick(&[eth, wifi, filter, dup, lo, vpn, wan, vswitch, bt, stale, direct, extra], &ips);
        let names: Vec<_> = got.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["Ethernet", "Wi-Fi", "WireGuard", "vEthernet (WSL)", "Bluetooth Network Connection"]);
        assert_eq!(
            (got[0].kind, got[0].speed_mbps, got[0].mac.as_deref(), got[0].mtu),
            (Kind::Wired, Some(1000), Some("01:02:03:04:05:06"), Some(1500))
        );
        assert_eq!(got[0].ipv4, ["192.168.1.2/24"]);
        assert_eq!((got[1].kind, got[1].up, got[1].speed_mbps), (Kind::Wireless, false, None));
        assert_eq!((got[2].kind, got[2].speed_mbps, got[2].mtu), (Kind::Vpn, None, None));
        assert_eq!(got[3].kind, Kind::Virtual);
        assert_eq!((got[4].kind, got[4].up), (Kind::Bluetooth, false));
    }

    #[test]
    fn the_hidden_adapters_are_read_from_their_class_keys() {
        let g = 0x6B29FC40_CA47_1067_B31D_00DD010662DA_u128;
        assert_eq!(parse_guid("{6B29FC40-CA47-1067-B31D-00DD010662DA}"), Some(g));
        assert_eq!(parse_guid("6b29fc40-ca47-1067-b31d-00dd010662da"), Some(g));
        assert_eq!(parse_guid(" {6b29fc40-ca47-1067-b31d-00dd010662da} "), Some(g));
        assert_eq!(parse_guid("{6B29FC40-CA47-1067-B31D}"), None);
        assert_eq!(parse_guid("{+B29FC40-CA47-1067-B31D-00DD010662DA}"), None);
        assert_eq!(parse_guid(""), None);
        let set = hidden([
            // NCF_HIDDEN | NCF_PHYSICAL | NCF_HAS_UI, and the card itself without it.
            ("{6B29FC40-CA47-1067-B31D-00DD010662DA}".to_string(), 0x8c),
            ("{00000000-0000-0000-0000-000000000001}".to_string(), 0x84),
            ("not a guid".to_string(), 0x8),
        ]);
        assert_eq!(set, HashSet::from([g]));
    }

    #[test]
    fn hardware_interfaces_get_netadapter_leaves_out_are_hidden() {
        let wifi = Row { hardware: true, connector: true, ..row(9, 2, "Wi-Fi", 71) };
        // Extra ports of the same card: no class key, not listed.
        let extra = Row { hardware: true, connector: true, ..row(12, 3, "Wi-Fi 3", 71) };
        let extra2 = Row { hardware: true, connector: true, ..row(3, 4, "Wi-Fi 5", 71) };
        // Not present: dropped anyway, so not counted here.
        let stale = Row { hardware: true, connector: true, gone: true, ..row(14, 5, "Wi-Fi 2", 71) };
        // Software interfaces are left to the other rules.
        let tunnel = Row { oper_up: true, ..row(1, 6, "Teredo Tunneling Pseudo-Interface", 131) };
        let rows = [wifi, extra, extra2, stale, tunnel];
        assert_eq!(unlisted(&rows, &HashSet::from([2])), HashSet::from([3, 4]));
        assert_eq!(unlisted(&rows, &HashSet::from([2, 3, 4])), HashSet::new());
        // No answer hides nothing.
        assert_eq!(unlisted(&rows, &HashSet::new()), HashSet::new());
    }

    #[test]
    fn socket_table_numbers_convert() {
        assert_eq!(tcp_state(2), State::Listen);
        assert_eq!(tcp_state(5), State::Established);
        assert_eq!(tcp_state(11), State::TimeWait);
        assert_eq!(tcp_state(12), State::Close);
        // Port 443 in network order, as a little-endian machine reads it.
        assert_eq!(port(u32::from(443u16.to_be())), 443);
        assert_eq!(v4(u32::from_ne_bytes([127, 0, 0, 1])), "127.0.0.1".parse::<IpAddr>().unwrap());
        let mut mapped = [0u8; 16];
        mapped[10..].copy_from_slice(&[0xff, 0xff, 10, 0, 0, 1]);
        assert_eq!(v6(mapped), "10.0.0.1".parse::<IpAddr>().unwrap());

        let p = |n: u16| u32::from(n.to_be());
        let c = conn(Proto::Tcp, (v4(0), p(22)), Some((v4(0), 0)), Some(2), 1234);
        assert_eq!((c.local, c.remote, c.state, c.pid), ("0.0.0.0:22".parse().unwrap(), None, State::Listen, Some(1234)));
        let c = conn(Proto::Udp6, (v6([0; 16]), p(5353)), None, None, 0);
        assert_eq!((c.local, c.remote, c.state, c.pid), ("[::]:5353".parse().unwrap(), None, State::None, None));
    }
}
