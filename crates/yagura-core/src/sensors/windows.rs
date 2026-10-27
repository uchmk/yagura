//! Windows: folding the `Thermal Zone Information` counters into sensors.
//! The PDH calls are in `zones.rs`.

use super::{Kind, Sensor};

/// `\_TZ.TZ00` → `TZ00`: the ACPI path without its namespace.
pub fn label(instance: &str) -> &str {
    let s = instance.trim_start_matches('\\');
    s.strip_prefix("_TZ.").unwrap_or(s)
}

/// Degrees from Kelvin, kept only when it can be a real reading (a zone
/// with no sensor reads 0 K).
fn celsius(kelvin: f64) -> Option<f32> {
    let c = kelvin - 273.15;
    (c > -60.0 && c < 250.0).then_some(c as f32)
}

/// One sensor per zone from `High Precision Temperature` (tenths of a
/// Kelvin) or, where a zone lacks it, `Temperature` (whole Kelvin).
pub fn fold(precise: &[(String, f64)], whole: &[(String, f64)]) -> Vec<Sensor> {
    let mut out: Vec<Sensor> = Vec::new();
    let all = precise.iter().map(|(i, v)| (i, v / 10.0)).chain(whole.iter().map(|(i, v)| (i, *v)));
    for (inst, k) in all {
        if out.iter().any(|s| s.id == *inst) {
            continue;
        }
        let Some(value) = celsius(k) else { continue };
        out.push(Sensor {
            id: inst.clone(),
            chip_id: "acpi".into(),
            chip: "Thermal zone".into(),
            label: label(inst).to_string(),
            kind: Kind::Temp,
            value,
            source: format!("\\Thermal Zone Information({inst})"),
            ..Sensor::default()
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zone_names_lose_the_acpi_namespace() {
        assert_eq!(label("\\_TZ.TZ00"), "TZ00");
        assert_eq!(label("_TZ.THRM"), "THRM");
        assert_eq!(label("CPUZ"), "CPUZ");
    }

    #[test]
    fn precise_readings_win_and_empty_zones_drop() {
        let s = |i: &str, v: f64| (i.to_string(), v);
        let list =
            fold(&[s("\\_TZ.TZ01", 3231.0), s("\\_TZ.TZ02", 0.0)], &[s("\\_TZ.TZ01", 323.0), s("\\_TZ.TZ00", 300.0), s("\\_TZ.TZ03", 0.0)]);
        let got: Vec<&str> = list.iter().map(|x| x.label.as_str()).collect();
        assert_eq!(got, ["TZ00", "TZ01"]);
        assert!((list[0].value - 26.85).abs() < 0.01 && (list[1].value - 49.95).abs() < 0.01);
        assert_eq!(list[0].chip_id, "acpi");
        assert_eq!(list[1].source, "\\Thermal Zone Information(\\_TZ.TZ01)");
    }
}
