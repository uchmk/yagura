//! Linux: the hwmon chips in `/sys/class/hwmon` (`temp*_input` in
//! millidegrees, `fan*_input` in rpm, with their limits), and the thermal
//! zones in `/sys/class/thermal` where no chip has a temperature.

use std::fs;
use std::path::Path;

use super::{Kind, Note, Sensor};

/// A file's trimmed text.
fn text(path: &Path) -> Option<String> {
    let s = fs::read_to_string(path).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// A file's number; unreadable sensors fail with EIO or ENODATA.
fn num(path: &Path) -> Option<f64> {
    text(path)?.parse().ok()
}

/// Degrees from millidegrees, kept only when it can be a real reading (an
/// unset limit reads 0, -273 or 65535).
fn celsius(milli: f64) -> Option<f32> {
    let c = milli / 1000.0;
    (c > -60.0 && c < 250.0 && c != 0.0).then_some(c as f32)
}

/// Whether a device's name is a PCI address (`0000:03:00.0`).
pub fn is_pci(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() == 12
        && b.iter().enumerate().all(|(i, &c)| match i {
            4 | 7 => c == b':',
            10 => c == b'.',
            _ => c.is_ascii_hexdigit(),
        })
}

/// The numbers of the files named `{prefix}{N}{suffix}` in a directory,
/// sorted.
fn numbered(names: &[String], prefix: &str, suffix: &str) -> Vec<u32> {
    let mut out: Vec<u32> = names.iter().filter_map(|n| n.strip_prefix(prefix)?.strip_suffix(suffix)?.parse().ok()).collect();
    out.sort_unstable();
    out.dedup();
    out
}

fn names(dir: &Path) -> Vec<String> {
    fs::read_dir(dir).map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default()
}

/// One hwmon chip's sensors: temperatures, then fans.
pub fn chip(dir: &Path, chip_id: &str) -> Vec<Sensor> {
    let Some(name) = text(&dir.join("name")) else { return Vec::new() };
    let chip = match text(&dir.join("device/model")) {
        Some(model) => format!("{name} {model}"),
        None => name,
    };
    let pci = fs::canonicalize(dir.join("device"))
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()))
        .filter(|n| is_pci(n));
    let files = names(dir);
    let base = Sensor { chip_id: chip_id.into(), chip, pci, ..Sensor::default() };
    let mut out = Vec::new();
    for n in numbered(&files, "temp", "_input") {
        let input = dir.join(format!("temp{n}_input"));
        let Some(value) = num(&input).and_then(celsius) else { continue };
        let limit = |what: &str| num(&dir.join(format!("temp{n}_{what}"))).and_then(celsius).filter(|&c| c > 0.0);
        out.push(Sensor {
            id: format!("{chip_id}/temp{n}"),
            label: text(&dir.join(format!("temp{n}_label"))).unwrap_or_else(|| format!("temp{n}")),
            kind: Kind::Temp,
            value,
            high: limit("max"),
            crit: limit("crit"),
            source: input.display().to_string(),
            ..base.clone()
        });
    }
    for n in numbered(&files, "fan", "_input") {
        let input = dir.join(format!("fan{n}_input"));
        let Some(value) = num(&input).filter(|v| (0.0..100_000.0).contains(v)) else { continue };
        let limit = |what: &str| num(&dir.join(format!("fan{n}_{what}"))).filter(|&v| v > 0.0 && v < 100_000.0).map(|v| v as f32);
        out.push(Sensor {
            id: format!("{chip_id}/fan{n}"),
            label: text(&dir.join(format!("fan{n}_label"))).unwrap_or_else(|| format!("fan{n}")),
            kind: Kind::Fan,
            value: value as f32,
            high: limit("max"),
            min: limit("min"),
            source: input.display().to_string(),
            ..base.clone()
        });
    }
    out
}

/// The thermal zones, for machines whose chips have no driver.
pub fn zones(dir: &Path) -> Vec<Sensor> {
    let mut ns: Vec<u32> = names(dir).iter().filter_map(|n| n.strip_prefix("thermal_zone")?.parse().ok()).collect();
    ns.sort_unstable();
    ns.into_iter()
        .filter_map(|n| {
            let z = dir.join(format!("thermal_zone{n}"));
            let input = z.join("temp");
            let value = num(&input).and_then(celsius)?;
            let files = names(&z);
            let trip = |kind: &str| {
                numbered(&files, "trip_point_", "_type").into_iter().find_map(|k| {
                    (text(&z.join(format!("trip_point_{k}_type"))).as_deref() == Some(kind))
                        .then(|| num(&z.join(format!("trip_point_{k}_temp"))).and_then(celsius))
                        .flatten()
                })
            };
            Some(Sensor {
                id: format!("thermal_zone{n}"),
                chip_id: "thermal".into(),
                chip: "Thermal zone".into(),
                label: text(&z.join("type")).unwrap_or_else(|| format!("zone{n}")),
                kind: Kind::Temp,
                value,
                high: trip("hot"),
                crit: trip("critical"),
                source: input.display().to_string(),
                ..Sensor::default()
            })
        })
        .collect()
}

/// Every chip under `root` (`/sys/class`) in hwmon order.
pub fn read(root: &Path) -> (Vec<Sensor>, Option<Note>) {
    let hwmon = root.join("hwmon");
    let mut ns: Vec<u32> = names(&hwmon).iter().filter_map(|n| n.strip_prefix("hwmon")?.parse().ok()).collect();
    ns.sort_unstable();
    let mut list: Vec<Sensor> = ns.into_iter().flat_map(|n| chip(&hwmon.join(format!("hwmon{n}")), &format!("hwmon{n}"))).collect();
    if list.iter().any(|s| s.kind == Kind::Temp) {
        return (list, None);
    }
    let zones = zones(&root.join("thermal"));
    let note = (!zones.is_empty()).then_some(Note::ThermalOnly);
    list.extend(zones);
    (list, note)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(dir: &Path, files: &[(&str, &str)]) {
        for (name, body) in files {
            let p = dir.join(name);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, format!("{body}\n")).unwrap();
        }
    }

    #[test]
    fn pci_addresses_are_told_from_other_devices() {
        assert!(is_pci("0000:03:00.0"));
        assert!(is_pci("0000:c1:00.1"));
        assert!(!is_pci("coretemp.0"));
        assert!(!is_pci("nvme0"));
        assert!(!is_pci("0000:03:00"));
    }

    #[test]
    fn hwmon_chips_give_temperatures_then_fans_with_their_limits() {
        let root = crate::util::test_dir("sensors-hwmon");
        let h = root.join("hwmon");
        put(
            &h.join("hwmon0"),
            &[
                ("name", "coretemp"),
                ("temp1_input", "45000"),
                ("temp1_label", "Package id 0"),
                ("temp1_max", "80000"),
                ("temp1_crit", "100000"),
                ("temp10_input", "47500"),
                ("temp2_input", "41000"),
                ("temp2_crit", "0"),
            ],
        );
        put(
            &h.join("hwmon10"),
            &[("name", "nct6798"), ("fan2_input", "0"), ("fan1_input", "1200"), ("fan1_min", "300"), ("fan1_label", "CPU fan")],
        );
        put(&h.join("hwmon2"), &[("name", "nvme"), ("device/model", "Samsung SSD 980"), ("temp1_input", "38850")]);
        // A chip whose sensor cannot be read, and one with no name.
        put(&h.join("hwmon3"), &[("name", "acpi_fan"), ("temp1_input", "")]);
        put(&h.join("hwmon4"), &[("temp1_input", "50000")]);
        let (list, note) = read(&root);
        assert_eq!(note, None);
        let ids: Vec<&str> = list.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["hwmon0/temp1", "hwmon0/temp2", "hwmon0/temp10", "hwmon2/temp1", "hwmon10/fan1", "hwmon10/fan2"]);
        let pkg = &list[0];
        assert_eq!((pkg.label.as_str(), pkg.value, pkg.high, pkg.crit), ("Package id 0", 45.0, Some(80.0), Some(100.0)));
        assert_eq!((list[1].label.as_str(), list[1].crit), ("temp2", None));
        assert_eq!(list[3].chip, "nvme Samsung SSD 980");
        assert_eq!(list[3].chip_id, "hwmon2");
        let fan = &list[4];
        assert_eq!((fan.kind, fan.label.as_str(), fan.value, fan.min), (Kind::Fan, "CPU fan", 1200.0, Some(300.0)));
        assert_eq!((list[5].label.as_str(), list[5].value), ("fan2", 0.0));
        assert!(fan.source.ends_with("fan1_input"));
    }

    #[test]
    fn thermal_zones_stand_in_when_no_chip_has_a_temperature() {
        let root = crate::util::test_dir("sensors-thermal");
        put(&root.join("hwmon/hwmon0"), &[("name", "pwmfan"), ("fan1_input", "3000")]);
        let t = root.join("thermal");
        put(
            &t.join("thermal_zone1"),
            &[
                ("type", "gpu-thermal"),
                ("temp", "52300"),
                ("trip_point_0_type", "passive"),
                ("trip_point_0_temp", "85000"),
                ("trip_point_1_type", "critical"),
                ("trip_point_1_temp", "110000"),
            ],
        );
        put(&t.join("thermal_zone0"), &[("type", "cpu-thermal"), ("temp", "48000")]);
        put(&t.join("cooling_device0"), &[("type", "pwm-fan")]);
        let (list, note) = read(&root);
        assert_eq!(note, Some(Note::ThermalOnly));
        let ids: Vec<&str> = list.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["hwmon0/fan1", "thermal_zone0", "thermal_zone1"]);
        let gpu = &list[2];
        assert_eq!((gpu.label.as_str(), gpu.value, gpu.high, gpu.crit), ("gpu-thermal", 52.3, None, Some(110.0)));
        // Nothing at all: no note here; the reader says Empty.
        assert_eq!(read(&crate::util::test_dir("sensors-none")), (Vec::new(), None));
    }
}
