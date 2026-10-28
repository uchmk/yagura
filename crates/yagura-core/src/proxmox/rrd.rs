//! The history `pvesh get .../rrddata` keeps for a node or a guest: one point
//! a minute for the last hour. Nothing here knows about GPUs; Proxmox does not
//! record them.

use std::process::Command;

use serde_json::Value;

use super::Kind;

/// How far back a graph reaches (`rrddata --timeframe`). Proxmox keeps coarser points the further back it goes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Timeframe {
    #[default]
    Hour,
    Day,
    Week,
    Month,
    Year,
}

impl Timeframe {
    /// The value `pvesh` takes.
    pub fn arg(self) -> &'static str {
        match self {
            Timeframe::Hour => "hour",
            Timeframe::Day => "day",
            Timeframe::Week => "week",
            Timeframe::Month => "month",
            Timeframe::Year => "year",
        }
    }

    /// The span in a graph's corner (`-7d`).
    pub fn short(self) -> &'static str {
        match self {
            Timeframe::Hour => "1h",
            Timeframe::Day => "1d",
            Timeframe::Week => "7d",
            Timeframe::Month => "30d",
            Timeframe::Year => "1y",
        }
    }

    /// The next longer span, and back to an hour after a year.
    pub fn next(self) -> Timeframe {
        match self {
            Timeframe::Hour => Timeframe::Day,
            Timeframe::Day => Timeframe::Week,
            Timeframe::Week => Timeframe::Month,
            Timeframe::Month => Timeframe::Year,
            Timeframe::Year => Timeframe::Hour,
        }
    }
}

/// One minute of history. Rates are bytes per second, as `rrddata` gives them.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    /// 0.0 to 1.0 of all the CPUs.
    pub cpu: f64,
    pub iowait: f64,
    pub mem: f64,
    pub maxmem: f64,
    /// The root disk of a node, the rootfs of a container; a VM reports none.
    pub disk: f64,
    pub maxdisk: f64,
    pub netin: f64,
    pub netout: f64,
    pub diskread: f64,
    pub diskwrite: f64,
}

/// The points of an `rrddata` answer, oldest first. A node and a guest name
/// the memory and disk fields differently; a missing (`null`) value is 0.
pub fn parse(json: &str) -> Vec<Point> {
    let Ok(Value::Array(rows)) = serde_json::from_str::<Value>(json) else { return Vec::new() };
    rows.iter()
        .filter(|r| r.get("time").is_some())
        .map(|r| {
            let f = |keys: &[&str]| keys.iter().find_map(|k| r.get(*k).and_then(Value::as_f64)).unwrap_or(0.0);
            Point {
                cpu: f(&["cpu"]),
                iowait: f(&["iowait"]),
                mem: f(&["mem", "memused"]),
                maxmem: f(&["maxmem", "memtotal"]),
                disk: f(&["disk", "rootused"]),
                maxdisk: f(&["maxdisk", "roottotal"]),
                netin: f(&["netin"]),
                netout: f(&["netout"]),
                diskread: f(&["diskread"]),
                diskwrite: f(&["diskwrite"]),
            }
        })
        .collect()
}

fn pvesh(path: &str, span: Timeframe) -> Vec<Point> {
    match Command::new("pvesh").args(["get", path, "--timeframe", span.arg(), "--output-format", "json"]).output() {
        Ok(o) if o.status.success() => parse(&String::from_utf8_lossy(&o.stdout)),
        _ => Vec::new(),
    }
}

/// A node's history over `span`. Runs `pvesh`: call it from a thread of its own.
pub fn node(name: &str, span: Timeframe) -> Vec<Point> {
    pvesh(&format!("/nodes/{name}/rrddata"), span)
}

/// A guest's history over `span`. Runs `pvesh`: call it from a thread of its own.
pub fn guest(node: &str, kind: Kind, id: u32, span: Timeframe) -> Vec<Point> {
    pvesh(&format!("/nodes/{node}/{}/{id}/rrddata", if kind == Kind::Lxc { "lxc" } else { "qemu" }), span)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_guest_row_is_read_and_a_missing_value_is_zero() {
        let j = r#"[{"time":1,"cpu":null},{"time":2,"cpu":0.25,"mem":512,"maxmem":1024,"disk":10,"maxdisk":100,"netin":5.5,"netout":1,"diskread":2,"diskwrite":3}]"#;
        let p = parse(j);
        assert_eq!(p.len(), 2);
        assert_eq!(p[0], Point::default());
        assert_eq!((p[1].cpu, p[1].mem, p[1].maxmem, p[1].disk, p[1].netin, p[1].diskwrite), (0.25, 512.0, 1024.0, 10.0, 5.5, 3.0));
    }

    #[test]
    fn a_node_row_uses_its_own_names() {
        let p = parse(r#"[{"time":1,"cpu":0.1,"iowait":0.02,"memused":8,"memtotal":16,"rootused":3,"roottotal":30}]"#);
        assert_eq!((p[0].mem, p[0].maxmem, p[0].disk, p[0].maxdisk, p[0].iowait), (8.0, 16.0, 3.0, 30.0, 0.02));
    }

    #[test]
    fn the_span_steps_round_from_an_hour_to_a_year() {
        let mut s = Timeframe::default();
        let mut seen = vec![s.arg()];
        for _ in 0..4 {
            s = s.next();
            seen.push(s.arg());
        }
        assert_eq!(seen, ["hour", "day", "week", "month", "year"]);
        assert_eq!(s.short(), "1y");
        assert_eq!(s.next(), Timeframe::Hour);
    }

    #[test]
    fn rubbish_is_no_points() {
        assert!(parse("nope").is_empty());
        assert!(parse(r#"{"a":1}"#).is_empty());
    }
}
