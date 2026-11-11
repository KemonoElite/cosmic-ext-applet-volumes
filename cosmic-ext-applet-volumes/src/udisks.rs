// SPDX-License-Identifier: GPL-3.0-only

//! UDisks2 backend.
//!
//! Everything the applet knows about drives comes from the `org.freedesktop.UDisks2`
//! service on the system bus, the same service `udisksctl`, GNOME Disks and COSMIC Files
//! use. Mounting, unmounting and ejecting go through it too, so polkit decides who is
//! allowed to do what instead of the applet shelling out to `umount` and hoping.

use std::{
    collections::HashMap,
    ffi::OsString,
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
};

use zbus::{Connection, OwnedObjectPath, OwnedVariant, Variant, proxy};

pub const SERVICE: &str = "org.freedesktop.UDisks2";
pub const ROOT: &str = "/org/freedesktop/UDisks2";

const IFACE_BLOCK: &str = "org.freedesktop.UDisks2.Block";
const IFACE_FILESYSTEM: &str = "org.freedesktop.UDisks2.Filesystem";
const IFACE_DRIVE: &str = "org.freedesktop.UDisks2.Drive";

/// `GetManagedObjects` reply: object path -> interface -> property -> value.
pub type Objects = HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedVariant>>>;
type Props = HashMap<String, OwnedVariant>;

#[proxy(
    interface = "org.freedesktop.UDisks2.Filesystem",
    default_service = "org.freedesktop.UDisks2"
)]
trait Filesystem {
    fn mount(&self, options: HashMap<&str, Variant<'_>>) -> zbus::Result<String>;
    fn unmount(&self, options: HashMap<&str, Variant<'_>>) -> zbus::Result<()>;
}

#[proxy(
    interface = "org.freedesktop.UDisks2.Drive",
    default_service = "org.freedesktop.UDisks2"
)]
trait Drive {
    fn eject(&self, options: HashMap<&str, Variant<'_>>) -> zbus::Result<()>;
    fn power_off(&self, options: HashMap<&str, Variant<'_>>) -> zbus::Result<()>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Usb,
    Optical,
    Card,
    Disk,
}

impl Kind {
    #[must_use]
    pub fn icon_name(self) -> &'static str {
        match self {
            Kind::Usb => "drive-harddisk-usb-symbolic",
            Kind::Optical => "media-optical-symbolic",
            Kind::Card => "media-flash-symbolic",
            Kind::Disk => "disks-symbolic",
        }
    }
}

/// The physical drive a volume lives on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveInfo {
    pub path: OwnedObjectPath,
    pub removable: bool,
    pub ejectable: bool,
    pub can_power_off: bool,
}

/// A mountable filesystem we want to show in the popup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    /// Object path of the block device; target of `Filesystem.Mount/Unmount`.
    pub path: OwnedObjectPath,
    /// Device node, e.g. `/dev/sdb1`.
    pub device: String,
    pub label: String,
    pub size: u64,
    pub kind: Kind,
    pub mount_point: Option<PathBuf>,
    pub drive: Option<DriveInfo>,
}

/// What to do to the drive itself once its filesystem has been unmounted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Release {
    Nothing,
    Eject(OwnedObjectPath),
    PowerOff(OwnedObjectPath),
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

fn get<T: TryFrom<OwnedVariant>>(props: &Props, key: &str) -> Option<T> {
    T::try_from(props.get(key)?.try_clone().ok()?).ok()
}

/// D-Bus byte strings (`ay`) are NUL terminated and are not guaranteed to be UTF-8.
fn bytes_to_path(mut bytes: Vec<u8>) -> PathBuf {
    if let Some(nul) = bytes.iter().position(|&b| b == 0) {
        bytes.truncate(nul);
    }
    PathBuf::from(OsString::from_vec(bytes))
}

fn is_none_path(path: &OwnedObjectPath) -> bool {
    path.as_str() == "/"
}

fn is_user_media_path(path: &Path) -> bool {
    path.starts_with("/run/media") || path.starts_with("/media")
}

fn drive_info(objects: &Objects, path: &OwnedObjectPath) -> Option<(DriveInfo, Kind)> {
    let props = objects.get(path)?.get(IFACE_DRIVE)?;
    let bus: String = get(props, "ConnectionBus").unwrap_or_default();
    let media: String = get(props, "Media").unwrap_or_default();
    let optical: bool = get(props, "Optical").unwrap_or(false);

    let kind = if optical {
        Kind::Optical
    } else if media.starts_with("flash_sd") || media.starts_with("flash_mmc") {
        Kind::Card
    } else if bus == "usb" {
        Kind::Usb
    } else {
        Kind::Disk
    };

    Some((
        DriveInfo {
            path: path.clone(),
            removable: get(props, "Removable").unwrap_or(false),
            ejectable: get(props, "Ejectable").unwrap_or(false),
            can_power_off: get(props, "CanPowerOff").unwrap_or(false),
        },
        kind,
    ))
}

/// Turn a UDisks2 object dump into the list of volumes worth showing, in a stable order.
///
/// A volume is shown when UDisks2 does not ask for it to be ignored and either it is not a
/// system device (USB sticks, SD cards, external disks) or the user has mounted it under
/// `/run/media` or `/media`. The second rule keeps internal disks the user mounted by hand,
/// which is what the original applet did with its path check.
#[must_use]
pub fn volumes(objects: &Objects) -> Vec<Volume> {
    let mut out = Vec::new();

    for (path, interfaces) in objects {
        let (Some(block), Some(fs)) = (
            interfaces.get(IFACE_BLOCK),
            interfaces.get(IFACE_FILESYSTEM),
        ) else {
            continue;
        };
        if get(block, "HintIgnore").unwrap_or(false) {
            continue;
        }

        let mount_point = get::<Vec<Vec<u8>>>(fs, "MountPoints")
            .and_then(|mps| mps.into_iter().next())
            .map(bytes_to_path);
        let user_mounted = mount_point.as_deref().is_some_and(is_user_media_path);
        if get(block, "HintSystem").unwrap_or(true) && !user_mounted {
            continue;
        }

        // Unlocked LUKS volumes have no drive of their own: follow them to the backing device.
        let mut drive_path: OwnedObjectPath = get(block, "Drive").unwrap_or_else(root_path);
        if is_none_path(&drive_path)
            && let Some(backing) = get::<OwnedObjectPath>(block, "CryptoBackingDevice")
            && let Some(props) = objects.get(&backing).and_then(|i| i.get(IFACE_BLOCK))
        {
            drive_path = get(props, "Drive").unwrap_or_else(root_path);
        }
        let (drive, kind) = match (!is_none_path(&drive_path))
            .then(|| drive_info(objects, &drive_path))
            .flatten()
        {
            Some((drive, kind)) => (Some(drive), kind),
            None => (None, Kind::Disk),
        };

        let device = get::<Vec<u8>>(block, "Device")
            .map(|b| bytes_to_path(b).to_string_lossy().into_owned())
            .unwrap_or_default();

        let file_name = |p: &Path| {
            p.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .filter(|n| !n.is_empty())
        };
        let label = get::<String>(block, "IdLabel")
            .filter(|l| !l.is_empty())
            .or_else(|| get::<String>(block, "HintName").filter(|l| !l.is_empty()))
            .or_else(|| mount_point.as_deref().and_then(file_name))
            .or_else(|| file_name(Path::new(&device)))
            .unwrap_or_else(|| device.clone());

        out.push(Volume {
            path: path.clone(),
            device,
            label,
            size: get(block, "Size").unwrap_or(0),
            kind,
            mount_point,
            drive,
        });
    }

    // HashMap iteration order is random; sort so the list never reshuffles and so that
    // unchanged snapshots compare equal.
    out.sort_by(|a, b| a.device.cmp(&b.device).then_with(|| a.path.cmp(&b.path)));
    out
}

fn root_path() -> OwnedObjectPath {
    OwnedObjectPath::try_from("/").expect("'/' is a valid object path")
}

/// Decide what, if anything, should happen to the drive after `volume` is unmounted.
///
/// Only removable drives are touched, and only when no other volume on the same drive is
/// still mounted. Drives that can be powered off are (the "safe to remove" case for USB);
/// otherwise ejectable media (optical discs, card readers) is ejected.
#[must_use]
pub fn release_plan(all: &[Volume], volume: &Volume) -> Release {
    let Some(drive) = volume.drive.as_ref().filter(|d| d.removable) else {
        return Release::Nothing;
    };
    let others_mounted = all.iter().any(|v| {
        v.path != volume.path
            && v.mount_point.is_some()
            && v.drive.as_ref().is_some_and(|d| d.path == drive.path)
    });
    if others_mounted {
        Release::Nothing
    } else if drive.can_power_off {
        Release::PowerOff(drive.path.clone())
    } else if drive.ejectable {
        Release::Eject(drive.path.clone())
    } else {
        Release::Nothing
    }
}

/// SI units, like GNOME Disks and COSMIC Files show them.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

// ---------------------------------------------------------------------------
// D-Bus calls
// ---------------------------------------------------------------------------

/// Human readable reason for a failed D-Bus call (the daemon's own message when it sent one).
#[must_use]
pub fn describe(err: &zbus::Error) -> String {
    match err {
        zbus::Error::MethodError(_, Some(description), _) => description.clone(),
        other => other.to_string(),
    }
}

/// # Errors
/// Fails when the system bus is unreachable or the UDisks2 service cannot be activated.
pub async fn fetch_objects(connection: &Connection) -> zbus::Result<Objects> {
    let manager = zbus::fdo::ObjectManagerProxy::builder(connection)
        .destination(SERVICE)
        .path(ROOT)
        .build()
        .await?;
    let dump = manager.get_managed_objects().await?;
    Ok(dump
        .into_iter()
        .map(|(path, interfaces)| {
            (
                path,
                interfaces
                    .into_iter()
                    .map(|(name, props)| (name.to_string(), props))
                    .collect(),
            )
        })
        .collect())
}

async fn filesystem<'a>(
    connection: &'a Connection,
    path: &OwnedObjectPath,
) -> zbus::Result<FilesystemProxy<'a>> {
    FilesystemProxy::builder(connection)
        .path(path.clone())
        .build()
        .await
}

/// Mount a volume and return where it ended up.
///
/// # Errors
/// The daemon's own message (polkit denial, unsupported filesystem, ...) as text.
pub async fn mount(path: OwnedObjectPath) -> Result<PathBuf, String> {
    async {
        let connection = Connection::system().await?;
        let target = filesystem(&connection, &path)
            .await?
            .mount(HashMap::new())
            .await?;
        Ok(PathBuf::from(target))
    }
    .await
    .map_err(|e: zbus::Error| describe(&e))
}

/// Unmount a volume (if it is mounted), then release the drive as decided by [`release_plan`].
///
/// # Errors
/// The daemon's own message (device busy, polkit denial, ...) as text.
pub async fn remove(path: OwnedObjectPath, mounted: bool, release: Release) -> Result<(), String> {
    async {
        let connection = Connection::system().await?;
        if mounted {
            filesystem(&connection, &path)
                .await?
                .unmount(HashMap::new())
                .await?;
        }
        match release {
            Release::Nothing => {}
            Release::Eject(drive) => {
                DriveProxy::builder(&connection)
                    .path(drive)
                    .build()
                    .await?
                    .eject(HashMap::new())
                    .await?;
            }
            Release::PowerOff(drive) => {
                DriveProxy::builder(&connection)
                    .path(drive)
                    .build()
                    .await?
                    .power_off(HashMap::new())
                    .await?;
            }
        }
        Ok(())
    }
    .await
    .map_err(|e: zbus::Error| describe(&e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn val(v: impl Into<Variant<'static>>) -> OwnedVariant {
        OwnedVariant::try_from(v.into()).expect("value without fds")
    }

    fn path(p: &str) -> OwnedObjectPath {
        OwnedObjectPath::try_from(p).unwrap()
    }

    /// NUL terminated byte string, as UDisks2 sends `Device` and `MountPoints` entries.
    fn cstr(s: &str) -> Vec<u8> {
        let mut b = s.as_bytes().to_vec();
        b.push(0);
        b
    }

    struct Dev<'a> {
        name: &'a str,
        label: &'a str,
        hint_name: &'a str,
        drive: &'a str,
        system: bool,
        ignore: bool,
        mount: Option<&'a str>,
    }

    impl<'a> Dev<'a> {
        fn new(name: &'a str) -> Self {
            Dev {
                name,
                label: "",
                hint_name: "",
                drive: "/org/freedesktop/UDisks2/drives/stick",
                system: false,
                ignore: false,
                mount: None,
            }
        }
    }

    /// UDisks2 escapes everything outside `[A-Za-z0-9_]` in object paths (`-` becomes `_2d`).
    fn block_path(name: &str) -> String {
        format!(
            "/org/freedesktop/UDisks2/block_devices/{}",
            name.replace('-', "_2d")
        )
    }

    fn add_device(objects: &mut Objects, dev: &Dev<'_>) {
        let mut block = Props::new();
        block.insert(
            "Device".into(),
            val(Variant::new(cstr(&format!("/dev/{}", dev.name)))),
        );
        block.insert("IdLabel".into(), val(dev.label.to_string()));
        block.insert("HintName".into(), val(dev.hint_name.to_string()));
        block.insert("Drive".into(), val(path(dev.drive)));
        block.insert("HintSystem".into(), val(dev.system));
        block.insert("HintIgnore".into(), val(dev.ignore));
        block.insert("Size".into(), val(16_000_000_000_u64));

        let mut fs = Props::new();
        let mounts: Vec<Vec<u8>> = dev.mount.iter().map(|m| cstr(m)).collect();
        fs.insert("MountPoints".into(), val(Variant::new(mounts)));

        let mut interfaces = HashMap::new();
        interfaces.insert(IFACE_BLOCK.to_string(), block);
        interfaces.insert(IFACE_FILESYSTEM.to_string(), fs);
        objects.insert(path(&block_path(dev.name)), interfaces);
    }

    fn add_drive(
        objects: &mut Objects,
        id: &str,
        bus: &str,
        media: &str,
        flags: (bool, bool, bool),
    ) {
        let (removable, ejectable, can_power_off) = flags;
        let mut props = Props::new();
        props.insert("ConnectionBus".into(), val(bus.to_string()));
        props.insert("Media".into(), val(media.to_string()));
        props.insert("Optical".into(), val(media.starts_with("optical")));
        props.insert("Removable".into(), val(removable));
        props.insert("Ejectable".into(), val(ejectable));
        props.insert("CanPowerOff".into(), val(can_power_off));
        let mut interfaces = HashMap::new();
        interfaces.insert(IFACE_DRIVE.to_string(), props);
        objects.insert(
            path(&format!("/org/freedesktop/UDisks2/drives/{id}")),
            interfaces,
        );
    }

    fn usb_stick_objects() -> Objects {
        let mut o = Objects::new();
        add_drive(&mut o, "stick", "usb", "", (true, true, true));
        o
    }

    #[test]
    fn shows_removable_hides_system_and_ignored_and_sorts() {
        let mut o = usb_stick_objects();
        add_device(
            &mut o,
            &Dev {
                label: "B",
                ..Dev::new("sdb2")
            },
        );
        add_device(
            &mut o,
            &Dev {
                label: "A",
                ..Dev::new("sdb1")
            },
        );
        add_device(
            &mut o,
            &Dev {
                system: true,
                ..Dev::new("nvme0n1p2")
            },
        );
        add_device(
            &mut o,
            &Dev {
                ignore: true,
                ..Dev::new("sdc1")
            },
        );

        let v = volumes(&o);
        assert_eq!(
            v.iter().map(|v| v.device.as_str()).collect::<Vec<_>>(),
            ["/dev/sdb1", "/dev/sdb2"]
        );
        assert_eq!(v[0].label, "A");
        assert_eq!(v[0].kind, Kind::Usb);
        assert_eq!(v[0].size, 16_000_000_000);
        assert!(v[0].mount_point.is_none());
    }

    #[test]
    fn output_is_deterministic_so_unchanged_snapshots_compare_equal() {
        let mut o = usb_stick_objects();
        for n in ["sdb1", "sdb2", "sdb3", "sdb4", "sdb5"] {
            add_device(&mut o, &Dev::new(n));
        }
        assert_eq!(volumes(&o), volumes(&o));
    }

    #[test]
    fn internal_disk_is_shown_only_when_mounted_under_media() {
        let mut o = Objects::new();
        add_device(
            &mut o,
            &Dev {
                drive: "/",
                system: true,
                ..Dev::new("sda1")
            },
        );
        assert!(volumes(&o).is_empty());

        add_device(
            &mut o,
            &Dev {
                drive: "/",
                system: true,
                mount: Some("/run/media/me/Data"),
                ..Dev::new("sda1")
            },
        );
        let v = volumes(&o);
        assert_eq!(v.len(), 1);
        assert_eq!(
            v[0].mount_point.as_deref(),
            Some(Path::new("/run/media/me/Data"))
        );
        assert_eq!(v[0].kind, Kind::Disk);
        assert!(v[0].drive.is_none());

        // Mounted somewhere that is not a user media location: still hidden (/, /boot, ...).
        add_device(
            &mut o,
            &Dev {
                drive: "/",
                system: true,
                mount: Some("/boot"),
                ..Dev::new("sda1")
            },
        );
        assert!(volumes(&o).is_empty());
    }

    #[test]
    fn label_falls_back_through_hint_name_mount_point_and_device() {
        let mut o = usb_stick_objects();
        add_device(
            &mut o,
            &Dev {
                label: "Photos",
                hint_name: "ignored",
                ..Dev::new("sdb1")
            },
        );
        add_device(
            &mut o,
            &Dev {
                hint_name: "Backup disk",
                ..Dev::new("sdb2")
            },
        );
        add_device(
            &mut o,
            &Dev {
                mount: Some("/run/media/me/from-mount"),
                ..Dev::new("sdb3")
            },
        );
        add_device(&mut o, &Dev::new("sdb4"));

        let labels: Vec<_> = volumes(&o).into_iter().map(|v| v.label).collect();
        assert_eq!(labels, ["Photos", "Backup disk", "from-mount", "sdb4"]);
    }

    #[test]
    fn mount_points_with_spaces_and_non_utf8_survive() {
        let mut o = usb_stick_objects();
        add_device(
            &mut o,
            &Dev {
                mount: Some("/run/media/me/My Stick"),
                ..Dev::new("sdb1")
            },
        );
        let v = volumes(&o);
        assert_eq!(
            v[0].mount_point.as_deref(),
            Some(Path::new("/run/media/me/My Stick"))
        );

        let raw = bytes_to_path(vec![b'/', b'm', 0xff, b'x', 0, b'j', b'u', b'n', b'k']);
        assert_eq!(raw.as_os_str().as_encoded_bytes(), b"/m\xffx");
    }

    #[test]
    fn kind_is_derived_from_drive_properties() {
        for (bus, media, expected) in [
            ("usb", "", Kind::Usb),
            ("usb", "flash_sd", Kind::Card),
            ("sdio", "flash_mmc", Kind::Card),
            ("ata", "optical_dvd", Kind::Optical),
            ("ata", "", Kind::Disk),
        ] {
            let mut o = Objects::new();
            add_drive(&mut o, "stick", bus, media, (true, true, false));
            add_device(&mut o, &Dev::new("sdb1"));
            assert_eq!(volumes(&o)[0].kind, expected, "bus={bus} media={media}");
        }
    }

    #[test]
    fn unlocked_luks_volume_inherits_drive_from_backing_device() {
        let mut o = usb_stick_objects();
        add_device(&mut o, &Dev::new("sdb1"));
        add_device(
            &mut o,
            &Dev {
                drive: "/",
                ..Dev::new("dm-0")
            },
        );
        let backing = path("/org/freedesktop/UDisks2/block_devices/sdb1");
        o.get_mut(&path(&block_path("dm-0")))
            .unwrap()
            .get_mut(IFACE_BLOCK)
            .unwrap()
            .insert("CryptoBackingDevice".into(), val(backing));

        let v = volumes(&o);
        let dm = v.iter().find(|v| v.device == "/dev/dm-0").unwrap();
        assert_eq!(dm.kind, Kind::Usb);
        assert!(dm.drive.as_ref().unwrap().removable);
    }

    fn volume_on(drive: &str, flags: (bool, bool, bool), mounted: bool, name: &str) -> Volume {
        let (removable, ejectable, power_off) = flags;
        Volume {
            path: path(&format!("/org/freedesktop/UDisks2/block_devices/{name}")),
            device: format!("/dev/{name}"),
            label: name.into(),
            size: 0,
            kind: Kind::Usb,
            mount_point: mounted.then(|| PathBuf::from("/run/media/me/x")),
            drive: Some(DriveInfo {
                path: path(&format!("/org/freedesktop/UDisks2/drives/{drive}")),
                removable,
                ejectable,
                can_power_off: power_off,
            }),
        }
    }

    #[test]
    fn release_plan_powers_off_usb_and_ejects_optical() {
        let usb = volume_on("stick", (true, true, true), true, "sdb1");
        assert_eq!(
            release_plan(std::slice::from_ref(&usb), &usb),
            Release::PowerOff(usb.drive.as_ref().unwrap().path.clone())
        );

        let disc = volume_on("cd", (true, true, false), true, "sr0");
        assert_eq!(
            release_plan(std::slice::from_ref(&disc), &disc),
            Release::Eject(disc.drive.as_ref().unwrap().path.clone())
        );
    }

    #[test]
    fn release_plan_leaves_drive_alone_when_not_safe_or_not_removable() {
        // A sibling partition on the same stick is still mounted.
        let a = volume_on("stick", (true, true, true), true, "sdb1");
        let b = volume_on("stick", (true, true, true), true, "sdb2");
        assert_eq!(release_plan(&[a.clone(), b.clone()], &a), Release::Nothing);

        // Once the sibling is unmounted it is safe to power off.
        let b_unmounted = Volume {
            mount_point: None,
            ..b
        };
        assert!(matches!(
            release_plan(&[a.clone(), b_unmounted], &a),
            Release::PowerOff(_)
        ));

        // Fixed disk mounted by the user: unmount only.
        let internal = volume_on("nvme", (false, false, false), true, "sda1");
        assert_eq!(
            release_plan(std::slice::from_ref(&internal), &internal),
            Release::Nothing
        );

        // No drive object at all (loop device etc.).
        let orphan = Volume {
            drive: None,
            ..internal
        };
        assert_eq!(
            release_plan(std::slice::from_ref(&orphan), &orphan),
            Release::Nothing
        );
    }

    #[test]
    fn sizes_use_si_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(999), "999 B");
        assert_eq!(human_size(1_000), "1.0 kB");
        assert_eq!(human_size(15_900_000_000), "15.9 GB");
        assert_eq!(human_size(2_000_000_000_000), "2.0 TB");
        assert_eq!(human_size(u64::MAX), "18446744.1 TB");
    }
}
