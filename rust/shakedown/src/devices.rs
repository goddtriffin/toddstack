//! The one device list: `devices.toml`, compiled in. Nothing else keeps a copy.

use serde::Deserialize;

use crate::error::Error;

/// One device: CSS pixels.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Device {
    /// The name projects pick it by.
    pub name: String,
    /// What it is.
    pub label: String,
    /// Viewport width in CSS pixels.
    pub width: u32,
    /// Viewport height in CSS pixels.
    pub height: u32,
    /// The real device's pixel density (native harnesses; web renders at 1).
    pub density: u32,
    /// Phones, foldables and tablets: touch and mobile emulation.
    pub mobile: bool,
}

#[derive(Deserialize)]
struct File {
    device: Vec<Device>,
}

const LIST: &str = include_str!("devices.toml");

/// Every device, in file order.
///
/// # Panics
///
/// Never in a released tool: the file is compiled in and a unit test parses it.
pub fn all() -> Vec<Device> {
    toml::from_str::<File>(LIST).map_or_else(|_| Vec::new(), |f| f.device)
}

/// The named devices, in the order named.
///
/// # Errors
///
/// An unknown name (lists the valid ones).
pub fn pick(names: &[String]) -> Result<Vec<Device>, Error> {
    let every = all();
    names
        .iter()
        .map(|n| {
            every.iter().find(|d| &d.name == n).cloned().ok_or_else(|| {
                Error::Config(format!(
                    "unknown device {n:?} (devices: {})",
                    every
                        .iter()
                        .map(|d| d.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })
        })
        .collect()
}

/// `shakedown devices`: the list as text.
pub fn table() -> String {
    all()
        .iter()
        .map(|d| {
            format!(
                "{:<20} {:>4}x{:<4} density {} {}  {}",
                d.name,
                d.width,
                d.height,
                d.density,
                if d.mobile { "mobile " } else { "desktop" },
                d.label
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_parses_and_has_every_infra_device() {
        let d = all();
        assert_eq!(d.len(), 18);
        for (name, w, h) in [
            ("desktop", 1440, 900),
            ("laptop", 1280, 720),
            ("tablet", 820, 1180),
            ("phone", 390, 844),
            ("small-phone", 375, 667),
            ("android-small", 360, 740),
            ("fold-folded", 344, 882),
            ("fold-open", 673, 841),
            ("duo-folded", 382, 580),
            ("duo-open", 669, 867),
            ("duo-folded-land", 594, 368),
        ] {
            let found = d.iter().find(|x| x.name == name).unwrap();
            assert_eq!((found.width, found.height), (w, h), "{name}");
        }
        let mut names: Vec<_> = d.iter().map(|x| &x.name).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), 18);
    }

    #[test]
    fn unknown_names_list_the_valid_ones() {
        let e = pick(&["nope".into()]).unwrap_err().to_string();
        assert!(e.contains("phone") && e.contains("nope"));
    }
}
