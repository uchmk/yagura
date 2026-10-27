//! Linux: the USB and PCI devices in sysfs (`/sys/bus/usb/devices`,
//! `/sys/bus/pci/devices`), named by the devices themselves (USB's
//! `product`) or by `usb.ids` / `pci.ids` when one is installed.
//!
//! Every read takes the root directory, so the tests build a fake sysfs.
//! Serial numbers are never read: the list goes to fleet viewers too.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use super::{Bus, Device, Devices, Kind, Named, Note, Status, lookup, pci_kind, usb_kind};

/// The names found in the ids tables, kept so a table is scanned again only
/// for a device not seen before.
#[derive(Default)]
pub struct Ids {
    /// By (USB?, vendor, device); unnamed pairs too, so they are not looked
    /// for again.
    names: HashMap<(bool, u16, u16), Named>,
}

impl Ids {
    /// Names the wanted pairs from `file` (`usb.ids` / `pci.ids`); false when
    /// no table is installed.
    fn fill(&mut self, root: &Path, file: &str, usb: bool, want: &[(u16, u16)]) -> bool {
        let Some(path) = ids_file(root, file) else { return false };
        let new: Vec<(u16, u16)> = want.iter().copied().filter(|&(v, d)| !self.names.contains_key(&(usb, v, d))).collect();
        if !new.is_empty() {
            // usb.ids has had Latin-1 in it.
            let text = fs::read(&path).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
            for ((v, d), n) in lookup(&text, &new) {
                self.names.insert((usb, v, d), n);
            }
        }
        true
    }

    fn get(&self, usb: bool, vendor: u16, device: u16) -> Named {
        self.names.get(&(usb, vendor, device)).cloned().unwrap_or_default()
    }
}

/// Where distributions put the ids tables.
fn ids_file(root: &Path, file: &str) -> Option<PathBuf> {
    ["usr/share/hwdata", "usr/share/misc", "usr/share"].iter().map(|d| root.join(d).join(file)).find(|p| p.is_file())
}

fn text(dir: &Path, name: &str) -> Option<String> {
    let s = fs::read_to_string(dir.join(name)).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

fn hex(dir: &Path, name: &str) -> Option<u32> {
    let s = text(dir, name)?;
    u32::from_str_radix(s.trim_start_matches("0x"), 16).ok()
}

/// The name of the driver bound to a device, from its `driver` link.
fn driver(dir: &Path) -> Option<String> {
    let link = fs::read_link(dir.join("driver")).ok()?;
    Some(link.file_name()?.to_string_lossy().into_owned())
}

/// The module's version, when it says one (`/sys/module/<driver>/version`).
fn version(root: &Path, driver: &str) -> String {
    if driver.is_empty() { String::new() } else { text(&root.join("sys/module").join(driver), "version").unwrap_or_default() }
}

/// The entries of a bus's `devices` directory, with where each one really is.
fn entries(dir: &Path) -> Option<Vec<(String, PathBuf)>> {
    let mut out: Vec<(String, PathBuf)> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let real = fs::canonicalize(e.path()).ok()?;
            Some((name, real))
        })
        .collect();
    out.sort();
    Some(out)
}

/// The device a sysfs path hangs off: the nearest directory above it whose
/// name is a listed device.
pub fn parent_in(path: &Path, known: &HashSet<String>) -> Option<String> {
    path.ancestors()
        .skip(1)
        .filter_map(|a| a.file_name())
        .map(|n| n.to_string_lossy())
        .find(|n| known.contains(n.as_ref()))
        .map(|n| n.into_owned())
}

/// A USB link speed as `speed` says it (`1.5`, `480`, `5000`) in kbit/s.
fn speed_kbps(s: &str) -> Option<u32> {
    s.parse::<f64>().ok().filter(|v| *v > 0.0).map(|v| (v * 1000.0).round() as u32)
}

pub fn read(root: &Path, ids: &mut Ids) -> Devices {
    let usb = entries(&root.join("sys/bus/usb/devices"));
    let pci = entries(&root.join("sys/bus/pci/devices"));
    if usb.is_none() && pci.is_none() {
        return Devices { list: Vec::new(), note: Some(Note::NoSysfs) };
    }
    let (usb, pci) = (usb.unwrap_or_default(), pci.unwrap_or_default());
    // Interfaces (`1-2:1.0`) are parts of their device, not devices.
    let (ifaces, usb): (Vec<_>, Vec<_>) = usb.into_iter().partition(|(n, _)| n.contains(':'));
    let known: HashSet<String> = usb.iter().chain(&pci).map(|(n, _)| n.clone()).collect();
    let mut list = Vec::with_capacity(usb.len() + pci.len());
    let mut unnamed = false;

    let pairs: Vec<(u16, u16)> = usb.iter().filter_map(|(_, p)| Some((hex(p, "idVendor")? as u16, hex(p, "idProduct")? as u16))).collect();
    let have_usb = ids.fill(root, "usb.ids", true, &pairs);
    for (name, path) in &usb {
        let (Some(vid), Some(pid)) = (hex(path, "idVendor"), hex(path, "idProduct")) else { continue };
        let (vid, pid) = (vid as u16, pid as u16);
        // An interface sits in its device's directory (a root hub `usb1`
        // holds `1-0:1.0`, so the names do not say it).
        let mine: Vec<&PathBuf> = ifaces.iter().filter(|(_, p)| p.parent() == Some(path.as_path())).map(|(_, p)| p).collect();
        let class = hex(path, "bDeviceClass").unwrap_or(0) as u8;
        // 00 (per interface), ef (miscellaneous) and ff (vendor): the
        // interfaces say what it is.
        let kind = match class {
            0x00 | 0xef | 0xff => {
                mine.iter().filter_map(|p| hex(p, "bInterfaceClass")).map(|c| usb_kind(c as u8)).find(|k| *k != Kind::Other)
            }
            c => Some(usb_kind(c)),
        }
        .unwrap_or(Kind::Other);
        // Every device has the generic `usb` driver; its interfaces' drivers
        // are the ones that do something.
        let mut drivers: Vec<String> = Vec::new();
        for d in mine.iter().filter_map(|p| driver(p)) {
            if !drivers.contains(&d) {
                drivers.push(d);
            }
        }
        if drivers.is_empty() {
            drivers.extend(driver(path).filter(|d| d != "usb"));
        }
        let named = ids.get(true, vid, pid);
        let product = text(path, "product");
        unnamed |= product.is_none() && named.device.is_none() && !have_usb;
        let status = if text(path, "authorized").as_deref() == Some("0") {
            Status::Disabled
        } else if drivers.is_empty() {
            Status::NoDriver
        } else {
            Status::Ok
        };
        let first = drivers.first().cloned().unwrap_or_default();
        list.push(Device {
            id: name.clone(),
            parent: parent_in(path, &known),
            name: product.or(named.device).unwrap_or_else(|| format!("USB {vid:04x}:{pid:04x}")),
            vendor: text(path, "manufacturer").or(named.vendor).unwrap_or_default(),
            kind,
            class_name: String::new(),
            driver_version: version(root, &first),
            driver: drivers.join(", "),
            status,
            bus: Bus::Usb {
                vid,
                pid,
                rev: hex(path, "bcdDevice").map(|r| r as u16),
                speed_kbps: text(path, "speed").and_then(|s| speed_kbps(&s)),
            },
            location: String::new(),
        });
    }

    let pairs: Vec<(u16, u16)> = pci.iter().filter_map(|(_, p)| Some((hex(p, "vendor")? as u16, hex(p, "device")? as u16))).collect();
    let have_pci = ids.fill(root, "pci.ids", false, &pairs);
    for (name, path) in &pci {
        let (Some(vendor), Some(device)) = (hex(path, "vendor"), hex(path, "device")) else { continue };
        let (vendor, device) = (vendor as u16, device as u16);
        let named = ids.get(false, vendor, device);
        unnamed |= named.device.is_none() && !have_pci;
        let drv = driver(path).unwrap_or_default();
        list.push(Device {
            id: name.clone(),
            parent: parent_in(path, &known),
            name: named.device.unwrap_or_else(|| format!("PCI {vendor:04x}:{device:04x}")),
            vendor: named.vendor.unwrap_or_default(),
            kind: hex(path, "class").map_or(Kind::Other, pci_kind),
            class_name: String::new(),
            driver_version: version(root, &drv),
            status: if drv.is_empty() { Status::NoDriver } else { Status::Ok },
            driver: drv,
            bus: Bus::Pci { vendor, device },
            location: String::new(),
        });
    }
    Devices { list, note: unnamed.then_some(Note::NoIds) }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn put(dir: &Path, files: &[(&str, &str)]) {
        fs::create_dir_all(dir).unwrap();
        for (k, v) in files {
            fs::write(dir.join(k), format!("{v}\n")).unwrap();
        }
    }

    /// A device under `sys/devices/…`, linked from its bus's list.
    fn dev(root: &Path, bus: &str, rel: &str, files: &[(&str, &str)], drv: Option<&str>) {
        let dir = root.join("sys/devices").join(rel);
        put(&dir, files);
        if let Some(d) = drv {
            let target = root.join("sys/bus").join(bus).join("drivers").join(d);
            fs::create_dir_all(&target).unwrap();
            symlink(target, dir.join("driver")).unwrap();
        }
        let list = root.join("sys/bus").join(bus).join("devices");
        fs::create_dir_all(&list).unwrap();
        symlink(&dir, list.join(dir.file_name().unwrap())).unwrap();
    }

    fn fake(root: &Path) {
        let xhci = "pci0000:00/0000:00:14.0";
        dev(root, "pci", xhci, &[("class", "0x0c0330"), ("vendor", "0x8086"), ("device", "0xa0ed")], Some("xhci_hcd"));
        dev(root, "pci", "pci0000:00/0000:00:1f.0", &[("class", "0x060100"), ("vendor", "0x8086"), ("device", "0xa082")], None);
        let hub = format!("{xhci}/usb1");
        let hub_files =
            [("idVendor", "1d6b"), ("idProduct", "0002"), ("bDeviceClass", "09"), ("product", "xHCI Host Controller"), ("speed", "480")];
        dev(root, "usb", &hub, &hub_files, Some("usb"));
        dev(root, "usb", &format!("{hub}/1-0:1.0"), &[("bInterfaceClass", "09")], Some("hub"));
        let rx = format!("{hub}/1-2");
        let rx_files = [("idVendor", "046d"), ("idProduct", "c52b"), ("bcdDevice", "1211"), ("bDeviceClass", "00"), ("speed", "12")];
        dev(root, "usb", &rx, &rx_files, Some("usb"));
        dev(root, "usb", &format!("{rx}/1-2:1.0"), &[("bInterfaceClass", "03")], Some("usbhid"));
        dev(root, "usb", &format!("{rx}/1-2:1.1"), &[("bInterfaceClass", "03")], Some("usbhid"));
        let off = [
            ("idVendor", "0781"),
            ("idProduct", "5581"),
            ("bDeviceClass", "00"),
            ("product", "Ultra"),
            ("authorized", "0"),
            ("speed", "1.5"),
        ];
        dev(root, "usb", &format!("{hub}/1-3"), &off, Some("usb"));
        put(&root.join("sys/module/usbhid"), &[("version", "2.6")]);
    }

    fn by_id<'a>(d: &'a Devices, id: &str) -> &'a Device {
        d.list.iter().find(|v| v.id == id).unwrap_or_else(|| panic!("{id} in {:?}", d.list))
    }

    #[test]
    fn sysfs_gives_usb_and_pci_devices_and_their_parents() {
        let root = crate::util::test_dir("devices-sysfs");
        fake(&root);
        let got = read(&root, &mut Ids::default());
        assert_eq!(got.list.len(), 5, "interfaces are not devices: {:?}", got.list);
        assert_eq!(got.note, Some(Note::NoIds));

        let rx = by_id(&got, "1-2");
        assert_eq!(rx.parent.as_deref(), Some("usb1"));
        assert_eq!(rx.name, "USB 046d:c52b");
        assert_eq!(rx.kind, Kind::Input, "the interfaces decide");
        assert_eq!(rx.driver, "usbhid");
        assert_eq!(rx.driver_version, "2.6");
        assert_eq!(rx.status, Status::Ok);
        assert_eq!(rx.bus, Bus::Usb { vid: 0x046d, pid: 0xc52b, rev: Some(0x1211), speed_kbps: Some(12_000) });

        let hub = by_id(&got, "usb1");
        assert_eq!(hub.parent.as_deref(), Some("0000:00:14.0"));
        assert_eq!((hub.name.as_str(), hub.kind, hub.driver.as_str()), ("xHCI Host Controller", Kind::Usb, "hub"));

        let off = by_id(&got, "1-3");
        assert_eq!(off.status, Status::Disabled);
        assert_eq!(off.bus, Bus::Usb { vid: 0x0781, pid: 0x5581, rev: None, speed_kbps: Some(1500) });
        assert_eq!(off.status, Status::Disabled);

        let xhci = by_id(&got, "0000:00:14.0");
        assert_eq!((xhci.kind, xhci.driver.as_str(), xhci.status, xhci.parent.as_deref()), (Kind::Usb, "xhci_hcd", Status::Ok, None));
        let isa = by_id(&got, "0000:00:1f.0");
        assert_eq!((isa.name.as_str(), isa.kind, isa.status), ("PCI 8086:a082", Kind::System, Status::NoDriver));
    }

    #[test]
    fn the_ids_tables_name_what_the_devices_do_not() {
        let root = crate::util::test_dir("devices-ids");
        fake(&root);
        put(
            &root.join("usr/share/hwdata"),
            &[
                ("usb.ids", "046d  Logitech, Inc.\n\tc52b  Unifying Receiver"),
                ("pci.ids", "8086  Intel Corporation\n\ta0ed  Tiger Lake USB"),
            ],
        );
        let mut ids = Ids::default();
        let got = read(&root, &mut ids);
        assert_eq!(got.note, None);
        let rx = by_id(&got, "1-2");
        assert_eq!((rx.name.as_str(), rx.vendor.as_str()), ("Unifying Receiver", "Logitech, Inc."));
        let xhci = by_id(&got, "0000:00:14.0");
        assert_eq!((xhci.name.as_str(), xhci.vendor.as_str()), ("Tiger Lake USB", "Intel Corporation"));
        // Known but not in the table: the vendor alone.
        assert_eq!(by_id(&got, "0000:00:1f.0").vendor, "Intel Corporation");
        // Kept: a second read does not need the table.
        fs::remove_file(root.join("usr/share/hwdata/usb.ids")).unwrap();
        fs::write(root.join("usr/share/hwdata/usb.ids"), "").unwrap();
        assert_eq!(by_id(&read(&root, &mut ids), "1-2").name, "Unifying Receiver");
    }

    #[test]
    fn no_sysfs_says_so() {
        let root = crate::util::test_dir("devices-none");
        assert_eq!(read(&root, &mut Ids::default()), Devices { list: Vec::new(), note: Some(Note::NoSysfs) });
    }

    #[test]
    fn speeds_read_in_kbps() {
        assert_eq!(speed_kbps("1.5"), Some(1500));
        assert_eq!(speed_kbps("5000"), Some(5_000_000));
        assert_eq!(speed_kbps("unknown"), None);
    }
}
