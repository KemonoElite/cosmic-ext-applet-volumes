// SPDX-License-Identifier: GPL-3.0-only

//! Android phones and other MTP devices, through GIO/GVfs.
//!
//! A phone does not expose a disk, so UDisks2 never sees it. It speaks MTP, which on Linux is
//! provided by GVfs (via libmtp) and shows up in GIO as a volume whose activation root is an
//! `mtp://` URI. That is the same route COSMIC Files and GNOME Files use.
//!
//! GIO objects are not `Send` and their signals need a GLib main loop, so all of it lives on one
//! dedicated thread. The rest of the applet talks to that thread through a [`Handle`] (commands)
//! and a stream of [`Event`]s (state), mirroring how `udisks` and `watch` work.

use std::{cell::RefCell, path::PathBuf, rc::Rc, thread, time::Duration};

use futures::{
    SinkExt, StreamExt,
    channel::{mpsc, oneshot},
};
use gio::{glib, prelude::*};

/// URI schemes GVfs uses for phones: MTP ("File transfer") and PTP ("Photo transfer", which
/// GVfs's gphoto2 backend serves).
const DEVICE_SCHEMES: &[&str] = &["mtp", "gphoto2"];

/// After a mount call returns, the monitor can take a moment to learn about the new mount.
const MOUNT_SETTLE_TRIES: u32 = 20;
const MOUNT_SETTLE_STEP: Duration = Duration::from_millis(100);

type Filter = fn(&gio::Volume) -> bool;

/// Where a mounted device can be opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    /// A real path (through GVfs's FUSE mount), which any file manager can open.
    Path(PathBuf),
    /// Only reachable through GIO, e.g. `mtp://Pixel_7_ABC123/`.
    Uri(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// Stable while the device stays connected (the activation root, which carries its serial).
    pub id: String,
    pub name: String,
    /// `Some` exactly when the device is mounted.
    pub location: Option<Location>,
}

#[derive(Debug, Clone)]
pub enum Event {
    /// Sent once, first: lets the applet issue commands.
    Ready(Handle),
    Devices(Vec<Device>),
}

enum Command {
    Mount(String, oneshot::Sender<Result<Location, String>>),
    Unmount(String, oneshot::Sender<Result<(), String>>),
}

/// Cheap to clone; talks to the GIO thread.
#[derive(Debug, Clone)]
pub struct Handle {
    commands: mpsc::UnboundedSender<Command>,
}

fn gone() -> String {
    "The device service stopped".to_string()
}

impl Handle {
    /// Mount a device and report where it can be opened.
    ///
    /// # Errors
    /// GVfs's own message, e.g. when the phone is locked or not in file transfer mode.
    pub async fn mount(&self, id: String) -> Result<Location, String> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .unbounded_send(Command::Mount(id, reply))
            .map_err(|_| gone())?;
        answer.await.unwrap_or_else(|_| Err(gone()))
    }

    /// Unmount a device (it stays connected and can be mounted again).
    ///
    /// # Errors
    /// GVfs's own message.
    pub async fn unmount(&self, id: String) -> Result<(), String> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .unbounded_send(Command::Unmount(id, reply))
            .map_err(|_| gone())?;
        answer.await.unwrap_or_else(|_| Err(gone()))
    }
}

/// Runs for as long as the receiver is alive.
pub async fn run(output: mpsc::Sender<Event>) {
    run_filtered(output, is_device).await;
}

/// [`run`] with a custom notion of which GIO volumes to show (used by tests).
pub async fn run_filtered(mut output: mpsc::Sender<Event>, filter: Filter) {
    let (command_tx, command_rx) = mpsc::unbounded();
    let (event_tx, mut event_rx) = mpsc::unbounded();

    let spawned = thread::Builder::new()
        .name("gio-devices".to_string())
        .spawn(move || worker(&event_tx, command_rx, filter));
    if let Err(err) = spawned {
        tracing::warn!("cannot start the GIO thread, phones will not be listed: {err}");
        return;
    }

    let handle = Handle {
        commands: command_tx,
    };
    if output.send(Event::Ready(handle)).await.is_err() {
        return;
    }
    while let Some(event) = event_rx.next().await {
        if output.send(event).await.is_err() {
            return;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Everything below runs on the GIO thread.
// ---------------------------------------------------------------------------------------------

fn worker(
    events: &mpsc::UnboundedSender<Event>,
    mut commands: mpsc::UnboundedReceiver<Command>,
    filter: Filter,
) {
    // GIO's volume monitors deliver their signals through the process-wide default main context
    // (a private context never sees them), and nothing else in this process runs one, so this
    // thread takes it over and runs it, which is what any GLib program's main thread does.
    let context = glib::MainContext::default();
    let main_loop = glib::MainLoop::new(Some(&context), false);

    let outcome = context.acquire().map(|_owner| {
        let monitor = gio::VolumeMonitor::get();
        let last: Rc<RefCell<Option<Vec<Device>>>> = Rc::default();

        // Take a snapshot and send it if it differs from the previous one.
        let publish = {
            let monitor = monitor.clone();
            let events = events.clone();
            let main_loop = main_loop.clone();
            Rc::new(move || {
                let devices = snapshot(&monitor, filter);
                if last.borrow().as_ref() == Some(&devices) {
                    return;
                }
                *last.borrow_mut() = Some(devices.clone());
                if events.unbounded_send(Event::Devices(devices)).is_err() {
                    main_loop.quit();
                }
            })
        };

        macro_rules! refresh_on {
            ($($connect:ident),+) => {$({
                let publish = publish.clone();
                monitor.$connect(move |_, _| publish());
            })+};
        }
        refresh_on!(
            connect_volume_added,
            connect_volume_removed,
            connect_volume_changed,
            connect_mount_added,
            connect_mount_removed,
            connect_mount_changed
        );

        {
            let monitor = monitor.clone();
            let publish = publish.clone();
            let main_loop = main_loop.clone();
            glib::spawn_future_local(async move {
                while let Some(command) = commands.next().await {
                    // One task per command: a phone that hangs while mounting must not block
                    // unmounting another.
                    let monitor = monitor.clone();
                    let publish = publish.clone();
                    glib::spawn_future_local(async move {
                        execute(&monitor, filter, command).await;
                        publish();
                    });
                }
                main_loop.quit();
            });
        }

        publish();
        main_loop.run();
    });

    if let Err(err) = outcome {
        tracing::error!("cannot run the GLib main context, phones will not be listed: {err}");
    }
}

/// True for volumes GVfs serves over MTP or PTP.
fn is_device(volume: &gio::Volume) -> bool {
    volume
        .activation_root()
        .and_then(|root| root.uri_scheme())
        .is_some_and(|scheme| is_device_scheme(&scheme))
}

fn is_device_scheme(scheme: &str) -> bool {
    DEVICE_SCHEMES
        .iter()
        .any(|device| device.eq_ignore_ascii_case(scheme))
}

fn device_id(volume: &gio::Volume) -> String {
    volume
        .activation_root()
        .map(|root| root.uri().to_string())
        .or_else(|| volume.uuid().map(|uuid| uuid.to_string()))
        .unwrap_or_else(|| volume.name().to_string())
}

fn location_of(root: &gio::File) -> Location {
    root.path()
        .map_or_else(|| Location::Uri(root.uri().to_string()), Location::Path)
}

fn snapshot(monitor: &gio::VolumeMonitor, filter: Filter) -> Vec<Device> {
    let mut devices: Vec<Device> = monitor
        .volumes()
        .iter()
        .filter(|volume| filter(volume))
        .map(|volume| Device {
            id: device_id(volume),
            name: volume.name().to_string(),
            location: volume.get_mount().map(|mount| location_of(&mount.root())),
        })
        .collect();
    // The monitor's order is not guaranteed; keep the list (and the diff against the last one)
    // stable.
    devices.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
    devices
}

fn find(monitor: &gio::VolumeMonitor, filter: Filter, id: &str) -> Result<gio::Volume, String> {
    monitor
        .volumes()
        .into_iter()
        .find(|volume| filter(volume) && device_id(volume) == id)
        .ok_or_else(|| "The device is no longer connected".to_string())
}

async fn execute(monitor: &gio::VolumeMonitor, filter: Filter, command: Command) {
    match command {
        Command::Mount(id, reply) => {
            let _ = reply.send(mount(monitor, filter, &id).await);
        }
        Command::Unmount(id, reply) => {
            let _ = reply.send(unmount(monitor, filter, &id).await);
        }
    }
}

async fn mount(monitor: &gio::VolumeMonitor, filter: Filter, id: &str) -> Result<Location, String> {
    let volume = find(monitor, filter, id)?;

    if volume.get_mount().is_none() {
        match volume
            .mount_future(gio::MountMountFlags::NONE, None::<&gio::MountOperation>)
            .await
        {
            Ok(()) => {}
            Err(err) if err.matches(gio::IOErrorEnum::AlreadyMounted) => {}
            Err(err) => return Err(err.message().to_string()),
        }
    }

    // The mount call can return before the monitor has processed the new mount.
    for _ in 0..MOUNT_SETTLE_TRIES {
        if let Some(mount) = volume.get_mount() {
            return Ok(location_of(&mount.root()));
        }
        glib::timeout_future(MOUNT_SETTLE_STEP).await;
    }
    // Still not reported: the activation root is where the mount lives anyway.
    volume
        .activation_root()
        .map(|root| location_of(&root))
        .ok_or_else(|| "The device did not report a location after mounting".to_string())
}

async fn unmount(monitor: &gio::VolumeMonitor, filter: Filter, id: &str) -> Result<(), String> {
    let Some(mount) = find(monitor, filter, id)?.get_mount() else {
        return Ok(()); // already unmounted
    };
    mount
        .unmount_with_operation_future(gio::MountUnmountFlags::NONE, None::<&gio::MountOperation>)
        .await
        .map_err(|err| err.message().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_phone_schemes_count_as_devices() {
        assert!(is_device_scheme("mtp"));
        assert!(is_device_scheme("gphoto2"));
        assert!(is_device_scheme("MTP"));
        for other in ["", "file", "smb", "sftp", "afc", "mtpx", "trash"] {
            assert!(!is_device_scheme(other), "{other:?}");
        }
    }

    #[test]
    fn location_prefers_a_real_path_over_a_uri() {
        assert_eq!(
            location_of(&gio::File::for_path("/run/user/1000/gvfs/mtp:host=Pixel")),
            Location::Path("/run/user/1000/gvfs/mtp:host=Pixel".into())
        );
        // A GVfs location with no FUSE path (not mounted / gvfsd-fuse not running).
        assert_eq!(
            location_of(&gio::File::for_uri("mtp://Pixel_7_ABC123/")),
            Location::Uri("mtp://Pixel_7_ABC123/".into())
        );
    }

    #[test]
    fn commands_fail_cleanly_when_the_gio_thread_is_gone() {
        // Receiver dropped: the thread has stopped.
        let (commands, receiver) = mpsc::unbounded();
        drop(receiver);
        let handle = Handle { commands };
        let err = futures::executor::block_on(handle.mount("x".into())).unwrap_err();
        assert_eq!(err, gone());
        assert_eq!(
            futures::executor::block_on(handle.unmount("x".into())).unwrap_err(),
            gone()
        );

        // Command received but never answered (thread died mid-operation): must not hang.
        let (commands, mut receiver) = mpsc::unbounded();
        let handle = Handle { commands };
        let worker = std::thread::spawn(move || {
            let command = futures::executor::block_on(receiver.next());
            drop(command); // drops the reply sender without answering
        });
        let err = futures::executor::block_on(handle.mount("x".into())).unwrap_err();
        assert_eq!(err, gone());
        worker.join().unwrap();
    }
}
