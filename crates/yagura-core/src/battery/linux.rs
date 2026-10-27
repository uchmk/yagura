//! Linux: `/sys/class/power_supply`. Each battery says its `capacity` and
//! `status`; a mains adapter says whether it is `online`.

use super::Battery;

/// One supply's files, as read.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Supply {
    pub kind: String,
    pub capacity: Option<String>,
    pub status: Option<String>,
    pub online: Option<String>,
}

#[cfg(target_os = "linux")]
pub fn read() -> Option<Battery> {
    let dir = std::fs::read_dir("/sys/class/power_supply").ok()?;
    let file = |p: &std::path::Path, f: &str| std::fs::read_to_string(p.join(f)).ok();
    let supplies: Vec<Supply> = dir
        .flatten()
        .map(|e| {
            let p = e.path();
            Supply {
                kind: file(&p, "type").unwrap_or_default(),
                capacity: file(&p, "capacity"),
                status: file(&p, "status"),
                online: file(&p, "online"),
            }
        })
        .collect();
    fold(&supplies)
}

/// The batteries averaged; plugged in when a mains adapter is online or a
/// battery charges or is full. Batteries that say no capacity (a mouse's,
/// reported as `Device` scope, still says `Battery`) are left out.
pub fn fold(supplies: &[Supply]) -> Option<Battery> {
    let mut sum = 0.0;
    let mut n = 0;
    let mut charging = false;
    let mut full = false;
    let mut mains = false;
    for s in supplies {
        match s.kind.trim() {
            "Battery" => {
                let Some(pct) = s.capacity.as_deref().and_then(|c| c.trim().parse::<f32>().ok()) else { continue };
                sum += pct.clamp(0.0, 100.0);
                n += 1;
                match s.status.as_deref().map(str::trim) {
                    Some("Charging") => charging = true,
                    Some("Full") => full = true,
                    _ => {}
                }
            }
            "Mains" | "USB" => mains |= s.online.as_deref().is_some_and(|o| o.trim() == "1"),
            _ => {}
        }
    }
    (n > 0).then(|| Battery { pct: sum / n as f32, charging, plugged: mains || charging || full })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bat(cap: &str, status: &str) -> Supply {
        Supply { kind: "Battery\n".into(), capacity: Some(format!("{cap}\n")), status: Some(format!("{status}\n")), online: None }
    }

    fn ac(online: &str) -> Supply {
        Supply { kind: "Mains\n".into(), online: Some(format!("{online}\n")), ..Supply::default() }
    }

    #[test]
    fn batteries_are_averaged_and_mains_is_plugged() {
        assert_eq!(fold(&[]), None);
        assert_eq!(fold(&[ac("1")]), None, "a desktop has mains and no battery");
        assert_eq!(fold(&[bat("87", "Discharging"), ac("0")]), Some(Battery { pct: 87.0, charging: false, plugged: false }));
        assert_eq!(fold(&[bat("40", "Charging"), bat("60", "Charging")]), Some(Battery { pct: 50.0, charging: true, plugged: true }));
        assert_eq!(fold(&[bat("100", "Full")]), Some(Battery { pct: 100.0, charging: false, plugged: true }));
        // Plugged in but held back (a charge limit): not charging, plugged.
        assert_eq!(fold(&[bat("80", "Not charging"), ac("1")]), Some(Battery { pct: 80.0, charging: false, plugged: true }));
        // No capacity file: left out.
        let mouse = Supply { kind: "Battery".into(), status: Some("Discharging".into()), ..Supply::default() };
        assert_eq!(fold(&[mouse]), None);
    }
}
