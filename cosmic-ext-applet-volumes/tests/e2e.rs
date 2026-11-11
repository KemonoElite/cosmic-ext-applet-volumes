// SPDX-License-Identifier: GPL-3.0-only

//! End-to-end test of the D-Bus layer against a fake UDisks2 service.
//!
//! A private `dbus-daemon` stands in for the system bus (`DBUS_SYSTEM_BUS_ADDRESS`) and a
//! small in-process service exports the same object tree, interfaces and signals UDisks2
//! does. No root, no real disks, no polkit. The test is skipped when `dbus-daemon` is not
//! installed.

// The fake service mirrors D-Bus method signatures, which the `#[interface]` macro forces onto
// `&self` methods, ignored `options` arguments and `async` fns even when nothing is awaited.
#![allow(
    clippy::unused_self,
    clippy::used_underscore_binding,
    clippy::unused_async
)]

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};

use cosmic_ext_applet_volumes::{
    udisks::{self, Kind, Release, Volume},
    watch::{self, Event},
};
use futures::{StreamExt, channel::mpsc};
use zbus::{OwnedObjectPath, OwnedVariant, connection, fdo, object_server::SignalEmitter};

fn cstr(s: &str) -> Vec<u8> {
    let mut bytes = s.as_bytes().to_vec();
    bytes.push(0);
    bytes
}

fn object_path(p: &str) -> OwnedObjectPath {
    OwnedObjectPath::try_from(p).unwrap()
}

// ---- fake UDisks2 ---------------------------------------------------------------------

struct Block {
    device: &'static str,
    label: &'static str,
    drive: &'static str,
    system: bool,
    size: u64,
}

#[zbus::interface(name = "org.freedesktop.UDisks2.Block")]
impl Block {
    #[zbus(property)]
    fn device(&self) -> Vec<u8> {
        cstr(self.device)
    }
    #[zbus(property)]
    fn id_label(&self) -> String {
        self.label.to_string()
    }
    #[zbus(property)]
    fn hint_name(&self) -> String {
        String::new()
    }
    #[zbus(property)]
    fn hint_system(&self) -> bool {
        self.system
    }
    #[zbus(property)]
    fn hint_ignore(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn size(&self) -> u64 {
        self.size
    }
    #[zbus(property)]
    fn drive(&self) -> OwnedObjectPath {
        object_path(self.drive)
    }
}

struct Filesystem {
    label: &'static str,
    mount: Arc<Mutex<Option<String>>>,
    /// Makes `Unmount` fail like a real "target is busy".
    busy: bool,
}

#[zbus::interface(name = "org.freedesktop.UDisks2.Filesystem")]
impl Filesystem {
    #[zbus(property)]
    fn mount_points(&self) -> Vec<Vec<u8>> {
        self.mount.lock().unwrap().iter().map(|m| cstr(m)).collect()
    }

    async fn mount(
        &self,
        _options: HashMap<String, OwnedVariant>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<String> {
        let target = format!("/run/media/tester/{}", self.label);
        *self.mount.lock().unwrap() = Some(target.clone());
        self.mount_points_changed(&emitter).await?;
        Ok(target)
    }

    async fn unmount(
        &self,
        _options: HashMap<String, OwnedVariant>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        if self.busy {
            return Err(fdo::Error::Failed(
                "Error unmounting: target is busy".to_string(),
            ));
        }
        *self.mount.lock().unwrap() = None;
        self.mount_points_changed(&emitter).await?;
        Ok(())
    }
}

struct Drive {
    connection: zbus::Connection,
    path: &'static str,
    children: Vec<&'static str>,
}

#[zbus::interface(name = "org.freedesktop.UDisks2.Drive")]
impl Drive {
    #[zbus(property)]
    fn connection_bus(&self) -> String {
        "usb".to_string()
    }
    #[zbus(property)]
    fn media(&self) -> String {
        String::new()
    }
    #[zbus(property)]
    fn optical(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn removable(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn ejectable(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn can_power_off(&self) -> bool {
        true
    }

    async fn eject(&self, _options: HashMap<String, OwnedVariant>) -> fdo::Result<()> {
        Err(fdo::Error::Failed("eject not expected in this test".into()))
    }

    /// Unplugs the drive: its objects disappear, announced with `InterfacesRemoved`.
    async fn power_off(&self, _options: HashMap<String, OwnedVariant>) -> fdo::Result<()> {
        let connection = self.connection.clone();
        let path = self.path;
        let children = self.children.clone();
        tokio::spawn(async move {
            let server = connection.object_server();
            for child in children {
                let _ = server.remove::<Block, _>(child).await;
                let _ = server.remove::<Filesystem, _>(child).await;
            }
            let _ = server.remove::<Drive, _>(path).await;
        });
        Ok(())
    }
}

const STICK_BLOCK: &str = "/org/freedesktop/UDisks2/block_devices/sdb1";
const STICK_DRIVE: &str = "/org/freedesktop/UDisks2/drives/Stick";
const BUSY_BLOCK: &str = "/org/freedesktop/UDisks2/block_devices/sdc1";
const BUSY_DRIVE: &str = "/org/freedesktop/UDisks2/drives/Busy";
const SYSTEM_BLOCK: &str = "/org/freedesktop/UDisks2/block_devices/nvme0n1p2";

async fn serve_fake_udisks() -> zbus::Connection {
    let connection = connection::Builder::system()
        .name(udisks::SERVICE)
        .serve_at(udisks::ROOT, fdo::ObjectManager)
        .build()
        .await
        .expect("fake UDisks2 on the private bus");
    let server = connection.object_server();

    for (block_path, drive_path, device, label, busy) in [
        (STICK_BLOCK, STICK_DRIVE, "/dev/sdb1", "STICK", false),
        (BUSY_BLOCK, BUSY_DRIVE, "/dev/sdc1", "BUSY", true),
    ] {
        let mount = Arc::new(Mutex::new(None));
        server
            .at(
                block_path,
                Block {
                    device,
                    label,
                    drive: drive_path,
                    system: false,
                    size: 16_000_000_000,
                },
            )
            .await
            .unwrap();
        server
            .at(block_path, Filesystem { label, mount, busy })
            .await
            .unwrap();
        server
            .at(
                drive_path,
                Drive {
                    connection: connection.clone(),
                    path: drive_path,
                    children: vec![block_path],
                },
            )
            .await
            .unwrap();
    }

    // An internal disk: must never show up.
    server
        .at(
            SYSTEM_BLOCK,
            Block {
                device: "/dev/nvme0n1p2",
                label: "root",
                drive: "/",
                system: true,
                size: 500_000_000_000,
            },
        )
        .await
        .unwrap();
    server
        .at(
            SYSTEM_BLOCK,
            Filesystem {
                label: "root",
                mount: Arc::new(Mutex::new(Some("/".to_string()))),
                busy: true,
            },
        )
        .await
        .unwrap();

    connection
}

// ---- harness --------------------------------------------------------------------------

struct PrivateBus {
    child: Child,
    socket: PathBuf,
    config: PathBuf,
}

/// Minimal bus policy, written to a temp file so the test does not depend on where (or whether)
/// the distribution installs `session.conf` (it is not at `/usr/share` on NixOS), nor on any
/// service activation directories of the machine running the test.
fn bus_config(socket: &Path) -> String {
    format!(
        r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path={}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#,
        socket.display()
    )
}

impl PrivateBus {
    /// `None` only when `dbus-daemon` is not installed; any other failure is a test failure.
    fn start() -> Option<Self> {
        let dir = std::env::temp_dir();
        let id = std::process::id();
        let socket = dir.join(format!("volumes-e2e-bus-{id}"));
        let config = dir.join(format!("volumes-e2e-bus-{id}.conf"));
        let _ = std::fs::remove_file(&socket);
        std::fs::write(&config, bus_config(&socket)).expect("write bus config");

        let child = match Command::new("dbus-daemon")
            .arg(format!("--config-file={}", config.display()))
            .args(["--nofork", "--nopidfile"])
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
        {
            Ok(child) => child,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let _ = std::fs::remove_file(&config);
                return None;
            }
            Err(err) => panic!("cannot start dbus-daemon: {err}"),
        };
        let bus = Self {
            child,
            socket,
            config,
        };
        for _ in 0..100 {
            if bus.socket.exists() {
                return Some(bus);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!(
            "dbus-daemon started but never created {}",
            bus.socket.display()
        );
    }
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_file(&self.config);
    }
}

/// Wait for the first event satisfying `pick`, ignoring others, with a timeout.
async fn expect<T>(
    rx: &mut mpsc::Receiver<Event>,
    what: &str,
    secs: u64,
    mut pick: impl FnMut(Event) -> Option<T>,
) -> T {
    let wait = async {
        while let Some(event) = rx.next().await {
            if let Some(found) = pick(event) {
                return found;
            }
        }
        panic!("event stream ended while waiting for: {what}");
    };
    tokio::time::timeout(Duration::from_secs(secs), wait)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for: {what}"))
}

fn find<'a>(volumes: &'a [Volume], device: &str) -> Option<&'a Volume> {
    volumes.iter().find(|v| v.device == device)
}

#[test]
fn watches_mounts_and_removes_drives_over_dbus() {
    let Some(bus) = PrivateBus::start() else {
        eprintln!("skipping: dbus-daemon not available");
        return;
    };
    // Must happen before any threads exist (hence not inside the runtime).
    // SAFETY: single-threaded at this point; the test harness runs this one test body.
    unsafe {
        std::env::set_var(
            "DBUS_SYSTEM_BUS_ADDRESS",
            format!("unix:path={}", bus.socket.display()),
        );
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(scenario());
}

async fn scenario() {
    let (tx, mut rx) = mpsc::channel(16);
    tokio::spawn(watch::run(tx));

    // 1. No UDisks2 on the bus yet: the applet must say so instead of hanging or crashing.
    let reason = expect(
        &mut rx,
        "Unavailable before the service exists",
        10,
        |e| match e {
            Event::Unavailable(reason) => Some(reason),
            Event::Volumes(_) => None,
        },
    )
    .await;
    assert!(!reason.is_empty());

    // 2. The service appears: the watcher reconnects on its own and lists only removable
    //    volumes (the internal disk is hidden), in a stable order.
    let _service = serve_fake_udisks().await;
    let volumes = expect(&mut rx, "initial volume list", 20, |e| match e {
        Event::Volumes(v) => Some(v),
        Event::Unavailable(_) => None,
    })
    .await;
    assert_eq!(
        volumes
            .iter()
            .map(|v| v.device.as_str())
            .collect::<Vec<_>>(),
        ["/dev/sdb1", "/dev/sdc1"],
        "internal disk must be hidden, order stable"
    );
    let stick = find(&volumes, "/dev/sdb1").unwrap().clone();
    assert_eq!(stick.label, "STICK");
    assert_eq!(stick.kind, Kind::Usb);
    assert_eq!(stick.size, 16_000_000_000);
    assert!(stick.mount_point.is_none());
    let drive = stick
        .drive
        .clone()
        .expect("drive resolved through Block.Drive");
    assert!(drive.removable && drive.can_power_off);

    // 3. Mount through the same call the UI makes; the watcher must pick the change up from
    //    the PropertiesChanged signal without being asked.
    let mount_point = udisks::mount(stick.path.clone()).await.expect("mount");
    assert_eq!(mount_point, PathBuf::from("/run/media/tester/STICK"));
    let mounted = expect(&mut rx, "volume shown as mounted", 5, |e| match e {
        Event::Volumes(v) => find(&v, "/dev/sdb1")
            .filter(|s| s.mount_point.is_some())
            .cloned(),
        Event::Unavailable(_) => None,
    })
    .await;
    assert_eq!(mounted.mount_point.as_deref(), Some(mount_point.as_path()));

    // 4. A failing unmount reports the daemon's own message.
    let busy = find(&volumes, "/dev/sdc1").unwrap();
    let err = udisks::remove(busy.path.clone(), true, Release::Nothing)
        .await
        .expect_err("busy volume cannot be unmounted");
    assert!(err.contains("target is busy"), "got: {err}");

    // 5. Safely remove: unmount, then power the drive off; its volume disappears from the list.
    let plan = udisks::release_plan(&volumes, &mounted);
    assert_eq!(plan, Release::PowerOff(drive.path.clone()));
    udisks::remove(mounted.path.clone(), true, plan)
        .await
        .expect("unmount + power off");
    let remaining = expect(&mut rx, "stick gone after power off", 5, |e| match e {
        Event::Volumes(v) if find(&v, "/dev/sdb1").is_none() => Some(v),
        _ => None,
    })
    .await;
    assert_eq!(
        remaining
            .iter()
            .map(|v| v.device.as_str())
            .collect::<Vec<_>>(),
        ["/dev/sdc1"]
    );
}
