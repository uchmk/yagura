//! Linux: the logins from `utmp`, the power profile from the platform
//! profile (or else the CPU governor).

#[cfg(target_os = "linux")]
use super::Session;
use super::User;

/// glibc's `struct utmp` is 384 bytes on x86_64 and aarch64 alike.
const RECORD: usize = 384;
const USER_PROCESS: i16 = 7;

#[cfg(target_os = "linux")]
pub fn read() -> Session {
    let users = ["/run/utmp", "/var/run/utmp"].iter().find_map(|p| std::fs::read(p).ok()).map(|b| parse_utmp(&b)).unwrap_or_default();
    let text = |p: &str| std::fs::read_to_string(p).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let power = text("/sys/firmware/acpi/platform_profile")
        .or_else(|| text("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor"))
        .unwrap_or_default();
    Session { users, power }
}

/// A NUL-padded C string field.
fn field(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).trim().to_string()
}

/// The `USER_PROCESS` records of a utmp file.
pub fn parse_utmp(b: &[u8]) -> Vec<User> {
    b.as_chunks::<RECORD>()
        .0
        .iter()
        .filter(|r| i16::from_le_bytes([r[0], r[1]]) == USER_PROCESS)
        .map(|r| User {
            line: field(&r[8..40]),
            name: field(&r[44..76]),
            host: field(&r[76..332]),
            since: u64::from(u32::from_le_bytes([r[340], r[341], r[342], r[343]])),
        })
        .filter(|u| !u.name.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(kind: i16, line: &str, user: &str, host: &str, secs: u32) -> Vec<u8> {
        let mut r = vec![0u8; RECORD];
        r[..2].copy_from_slice(&kind.to_le_bytes());
        r[8..8 + line.len()].copy_from_slice(line.as_bytes());
        r[44..44 + user.len()].copy_from_slice(user.as_bytes());
        r[76..76 + host.len()].copy_from_slice(host.as_bytes());
        r[340..344].copy_from_slice(&secs.to_le_bytes());
        r
    }

    #[test]
    fn only_logins_are_listed() {
        let mut b = record(2, "~", "reboot", "6.1", 5);
        b.extend(record(7, "tty1", "ann", "", 1000));
        b.extend(record(8, "pts/0", "dead", "", 1));
        b.extend(record(7, "pts/1", "bob", "10.0.0.2", 2000));
        b.extend([1, 2, 3]);
        let u = parse_utmp(&b);
        assert_eq!(u.len(), 2);
        assert_eq!(u[0], User { name: "ann".into(), line: "tty1".into(), host: String::new(), since: 1000 });
        assert_eq!((u[1].name.as_str(), u[1].host.as_str()), ("bob", "10.0.0.2"));
    }

    #[test]
    fn nothing_is_nothing() {
        assert!(parse_utmp(&[]).is_empty());
    }
}
