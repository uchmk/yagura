//! Windows: LibreHardwareMonitor's WMI classes (`root\LibreHardwareMonitor`,
//! or `root\OpenHardwareMonitor` for its predecessor, with the same schema),
//! there while it runs. Like HWiNFO's shared memory, the tool reads the
//! chips through its own driver and yagura only reads what it publishes.
//!
//! [`fold`] turns the `Sensor` and `Hardware` rows into sensors and is pure;
//! [`Reader`] holds the WMI connection on the collect thread (COM in the
//! multithreaded apartment), drops it when a query fails and tries again
//! now and then, so a tool started later is found.

use super::{Kind, Sensor};

/// One row of the `Sensor` class.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Row {
    /// `/amdcpu/0/temperature/2`.
    pub id: String,
    /// `Core (Tctl/Tdie)`.
    pub name: String,
    /// `Temperature`, `Fan`, `Voltage`, …
    pub kind: String,
    pub value: f32,
    /// The hardware's identifier: `/amdcpu/0`.
    pub parent: String,
}

/// The temperatures and fans among `rows`, named by their hardware (an
/// identifier → name list from the `Hardware` class), each hardware's
/// together in the order the hardware is listed. `tool` names the source.
pub fn fold(rows: &[Row], hardware: &[(String, String)], tool: &str) -> Vec<Sensor> {
    let mut out: Vec<Sensor> = rows
        .iter()
        .filter_map(|r| {
            let kind = match r.kind.as_str() {
                "Temperature" => Kind::Temp,
                "Fan" => Kind::Fan,
                _ => return None,
            };
            // A sensor with nothing behind it reads NaN, or 0 for a fan header
            // with no fan on it, which is worth showing.
            if !r.value.is_finite() {
                return None;
            }
            let chip = hardware.iter().find(|h| h.0 == r.parent).map_or(r.parent.as_str(), |h| h.1.as_str());
            Some(Sensor {
                id: format!("lhm:{}", r.id),
                chip_id: format!("lhm:{}", r.parent),
                chip: chip.to_string(),
                label: r.name.clone(),
                kind,
                value: r.value,
                source: format!("{tool} {}", r.id),
                ..Sensor::default()
            })
        })
        .collect();
    let order = |s: &Sensor| (hardware.iter().position(|h| format!("lhm:{}", h.0) == s.chip_id).unwrap_or(usize::MAX), s.chip_id.clone());
    // Stable: a hardware's sensors stay in the tool's order (temperatures by index).
    out.sort_by_key(order);
    out
}

#[cfg(windows)]
pub use wmi::Reader;

#[cfg(windows)]
mod wmi {
    use std::time::{Duration, Instant};

    use ::windows::Win32::System::Wmi::IWbemServices;

    use super::{Row, Sensor, fold};
    use crate::wmi::{self as w, number, rows, text};

    /// How long to wait before looking for the tool again.
    const RETRY: Duration = Duration::from_secs(10);
    /// The namespaces, newest first, with the name shown as the source.
    const TOOLS: [(&str, &str); 2] =
        [("root\\LibreHardwareMonitor", "LibreHardwareMonitor"), ("root\\OpenHardwareMonitor", "OpenHardwareMonitor")];

    pub struct Reader {
        conn: Option<(IWbemServices, &'static str)>,
        hardware: Vec<(String, String)>,
        tried: Option<Instant>,
    }

    impl Reader {
        /// Starts COM on this (the collect) thread; connects on the first read.
        pub fn new() -> Self {
            w::init();
            Self { conn: None, hardware: Vec::new(), tried: None }
        }

        /// The sensors while the tool runs, `None` otherwise.
        pub fn read(&mut self) -> Option<Vec<Sensor>> {
            if self.conn.is_none() {
                if self.tried.is_some_and(|t| t.elapsed() < RETRY) {
                    return None;
                }
                self.tried = Some(Instant::now());
                self.conn = TOOLS.iter().find_map(|&(ns, tool)| w::connect(ns).map(|svc| (svc, tool)));
                self.hardware.clear();
            }
            let (svc, tool) = self.conn.as_ref()?;
            let rows = match sensors(svc) {
                Some(r) if !r.is_empty() => r,
                // The tool quit (its provider is gone) or lists nothing yet.
                _ => {
                    self.conn = None;
                    return None;
                }
            };
            if rows.iter().any(|r| !self.hardware.iter().any(|h| h.0 == r.parent)) {
                self.hardware = hardware(svc).unwrap_or_default();
            }
            Some(fold(&rows, &self.hardware, tool))
        }
    }

    fn sensors(svc: &IWbemServices) -> Option<Vec<Row>> {
        let q = "SELECT Identifier, Name, SensorType, Value, Parent FROM Sensor WHERE SensorType = 'Temperature' OR SensorType = 'Fan'";
        rows(svc, q, |o| {
            Some(Row {
                id: text(o, "Identifier")?,
                name: text(o, "Name")?,
                kind: text(o, "SensorType")?,
                value: number(o, "Value")?,
                parent: text(o, "Parent")?,
            })
        })
    }

    fn hardware(svc: &IWbemServices) -> Option<Vec<(String, String)>> {
        rows(svc, "SELECT Identifier, Name FROM Hardware", |o| Some((text(o, "Identifier")?, text(o, "Name")?)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, name: &str, kind: &str, value: f32, parent: &str) -> Row {
        Row { id: id.into(), name: name.into(), kind: kind.into(), value, parent: parent.into() }
    }

    #[test]
    fn temperatures_and_fans_by_hardware() {
        let hw = [("/amdcpu/0".to_string(), "AMD Ryzen 7 7800X3D".to_string()), ("/lpc/nct6799d/0".into(), "Nuvoton NCT6799D".into())];
        let rows = [
            row("/lpc/nct6799d/0/fan/1", "Fan #2", "Fan", 812.0, "/lpc/nct6799d/0"),
            row("/amdcpu/0/temperature/2", "Core (Tctl/Tdie)", "Temperature", 52.3, "/amdcpu/0"),
            row("/amdcpu/0/voltage/0", "Core (SVI2)", "Voltage", 1.1, "/amdcpu/0"),
            row("/lpc/nct6799d/0/fan/2", "Fan #3", "Fan", 0.0, "/lpc/nct6799d/0"),
            row("/lpc/nct6799d/0/temperature/0", "CPU Core", "Temperature", f32::NAN, "/lpc/nct6799d/0"),
            row("/gpu-nvidia/0/temperature/0", "GPU Core", "Temperature", 45.0, "/gpu-nvidia/0"),
        ];
        let list = fold(&rows, &hw, "LibreHardwareMonitor");
        let got: Vec<(&str, &str, Kind, f32)> = list.iter().map(|s| (s.chip.as_str(), s.label.as_str(), s.kind, s.value)).collect();
        assert_eq!(
            got,
            [
                ("AMD Ryzen 7 7800X3D", "Core (Tctl/Tdie)", Kind::Temp, 52.3),
                ("Nuvoton NCT6799D", "Fan #2", Kind::Fan, 812.0),
                ("Nuvoton NCT6799D", "Fan #3", Kind::Fan, 0.0),
                // Hardware the list does not name yet keeps its identifier.
                ("/gpu-nvidia/0", "GPU Core", Kind::Temp, 45.0),
            ]
        );
        assert_eq!((list[0].id.as_str(), list[0].chip_id.as_str()), ("lhm:/amdcpu/0/temperature/2", "lhm:/amdcpu/0"));
        assert_eq!(list[1].source, "LibreHardwareMonitor /lpc/nct6799d/0/fan/1");
    }
}
