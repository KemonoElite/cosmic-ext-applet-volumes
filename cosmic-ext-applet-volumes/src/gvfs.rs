// SPDX-License-Identifier: GPL-3.0-only

//! Phones (MTP) and network shares (Samba/SMB, NFS), through GIO/GVfs.
//!
//! None of these are disks, so UDisks2 never sees them. On Linux desktops they are GVfs's job:
//! phones show up in GIO as volumes with an `mtp://` activation root, shares as mounts of
//! `smb://` or `nfs://` locations (or as kernel `cifs`/`nfs` mounts, which GIO reports too). That
//! is the same route COSMIC Files and GNOME Files use.
//!
//! GIO objects are not `Send` and their signals need a GLib main loop, so all of it lives on one
//! dedicated thread. The rest of the applet talks to that thread through a [`Handle`] (commands)
//! and a stream of [`Event`]s (state), mirroring how `udisks` and `watch` work.

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    fmt,
    path::PathBuf,
    rc::Rc,
    thread,
    time::Duration,
};

use futures::{
    SinkExt, StreamExt,
    channel::{mpsc, oneshot},
};
use gio::{glib, prelude::*};

use crate::shares;

/// URI schemes GVfs uses for phones: MTP ("File transfer") and PTP ("Photo transfer", which
/// GVfs's gphoto2 backend serves).
const PHONE_SCHEMES: &[&str] = &["mtp", "gphoto2"];

/// After a mount call returns, the monitor can take a moment to learn about the new mount.
const MOUNT_SETTLE_TRIES: u32 = 20;
const MOUNT_SETTLE_STEP: Duration = Duration::from_millis(100);

type Classify = fn(&gio::Volume) -> Option<Kind>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Phone,
    /// A Samba or NFS share.
    Share,
}

/// Where a mounted device can be opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    /// A real path (through GVfs's FUSE mount, or a kernel mount), which any file manager can open.
    Path(PathBuf),
    /// Only reachable through GIO, e.g. `mtp://Pixel_7_ABC123/` or `smb://nas/media`.
    Uri(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// Stable while the device stays around: the activation root for phones, the address for
    /// shares, the device column for fstab entries.
    pub id: String,
    pub name: String,
    pub kind: Kind,
    /// `Some` exactly when the device is mounted.
    pub location: Option<Location>,
    /// A share the user added with "Connect to server", which can be forgotten again.
    pub saved: bool,
}

/// A password that does not end up in logs through `Debug`.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    #[must_use]
    pub fn new(text: String) -> Self {
        Self(text)
    }
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// GVfs wants credentials to finish a mount.
// The four flags mirror GIO's independent `AskPasswordFlags` one-to-one.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialRequest {
    pub request: u64,
    /// The device being mounted.
    pub device: String,
    pub message: String,
    pub username: bool,
    pub domain: bool,
    pub password: bool,
    /// Connecting without credentials is an option (guest shares).
    pub anonymous: bool,
    pub default_user: String,
    pub default_domain: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    pub username: String,
    pub domain: String,
    pub password: Secret,
    pub anonymous: bool,
}

#[derive(Debug, Clone)]
pub enum Event {
    /// Sent once, first: lets the applet issue commands.
    Ready(Handle),
    Devices(Vec<Device>),
    Credentials(CredentialRequest),
}

enum Command {
    Mount(String, oneshot::Sender<Result<Location, String>>),
    Unmount(String, oneshot::Sender<Result<(), String>>),
    /// The shares the user added; the worker lists them even when they are not mounted.
    SetSaved(Vec<String>),
    /// The user's answer to a [`CredentialRequest`]; `None` cancels.
    Answer(u64, Option<Credentials>),
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
    /// Mount a device or share and report where it can be opened. An empty error means the user
    /// cancelled and there is nothing to report.
    ///
    /// # Errors
    /// GVfs's own message, e.g. when the phone is locked or the server refuses the connection.
    pub async fn mount(&self, id: String) -> Result<Location, String> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .unbounded_send(Command::Mount(id, reply))
            .map_err(|_| gone())?;
        answer.await.unwrap_or_else(|_| Err(gone()))
    }

    /// Unmount a device or share (a phone stays connected and can be mounted again).
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

    pub fn set_saved(&self, shares: Vec<String>) {
        let _ = self.commands.unbounded_send(Command::SetSaved(shares));
    }

    pub fn answer(&self, request: u64, credentials: Option<Credentials>) {
        let _ = self
            .commands
            .unbounded_send(Command::Answer(request, credentials));
    }
}

/// Runs for as long as the receiver is alive.
pub async fn run(output: mpsc::Sender<Event>) {
    run_with(output, classify_volume).await;
}

/// [`run`] with a custom notion of which GIO volumes to show (used by tests).
pub async fn run_with(mut output: mpsc::Sender<Event>, classify: Classify) {
    let (command_tx, command_rx) = mpsc::unbounded();
    let (event_tx, mut event_rx) = mpsc::unbounded();

    let spawned = thread::Builder::new()
        .name("gio-devices".to_string())
        .spawn(move || worker(&event_tx, command_rx, classify));
    if let Err(err) = spawned {
        tracing::warn!("cannot start the GIO thread, phones and shares will not be listed: {err}");
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

/// What the GIO thread's callbacks share.
#[derive(Clone)]
struct Ctx {
    monitor: gio::VolumeMonitor,
    classify: Classify,
    saved: Rc<RefCell<Vec<String>>>,
    /// Mount operations waiting for the user's credentials, with the device they belong to.
    pending: Rc<RefCell<HashMap<u64, (gio::MountOperation, String)>>>,
    /// Devices whose prompt the user cancelled. The backend reports that as a generic failure
    /// ("Invalid argument" for SMB), which must not be shown as an error.
    cancelled: Rc<RefCell<HashSet<String>>>,
    next_request: Rc<Cell<u64>>,
    events: mpsc::UnboundedSender<Event>,
}

impl Ctx {
    /// A mount operation that turns GVfs's password prompt into an [`Event::Credentials`].
    fn operation(&self, device: &str) -> gio::MountOperation {
        let operation = gio::MountOperation::new();
        let ctx = self.clone();
        let device = device.to_string();
        operation.connect_ask_password(
            move |operation, message, default_user, default_domain, flags| {
                // The signal's default handler answers "unhandled" as soon as ours returns, which
                // would make GVfs give up before a person has typed anything (GTK's own mount
                // operation avoids this by overriding the handler). We answer later, from `answer`.
                operation.stop_signal_emission_by_name("ask-password");
                let request = ctx.next_request.get();
                ctx.next_request.set(request + 1);
                ctx.pending
                    .borrow_mut()
                    .insert(request, (operation.clone(), device.clone()));
                let _ = ctx
                    .events
                    .unbounded_send(Event::Credentials(CredentialRequest {
                        request,
                        device: device.clone(),
                        message: message.to_string(),
                        username: flags.contains(gio::AskPasswordFlags::NEED_USERNAME),
                        domain: flags.contains(gio::AskPasswordFlags::NEED_DOMAIN),
                        password: flags.contains(gio::AskPasswordFlags::NEED_PASSWORD),
                        anonymous: flags.contains(gio::AskPasswordFlags::ANONYMOUS_SUPPORTED),
                        default_user: default_user.to_string(),
                        default_domain: default_domain.to_string(),
                    }));
            },
        );
        operation
    }

    fn answer(&self, request: u64, credentials: Option<Credentials>) {
        let Some((operation, device)) = self.pending.borrow_mut().remove(&request) else {
            return;
        };
        let Some(credentials) = credentials else {
            self.cancelled.borrow_mut().insert(device);
            operation.reply(gio::MountOperationResult::Aborted);
            return;
        };
        if credentials.anonymous {
            operation.set_anonymous(true);
        } else {
            operation.set_username(Some(&credentials.username));
            if !credentials.domain.is_empty() {
                operation.set_domain(Some(&credentials.domain));
            }
            operation.set_password(Some(credentials.password.expose()));
        }
        // Lets GVfs reuse the password for this session instead of asking again; nothing is
        // written to disk.
        operation.set_password_save(gio::PasswordSave::ForSession);
        operation.reply(gio::MountOperationResult::Handled);
    }
}

fn worker(
    events: &mpsc::UnboundedSender<Event>,
    mut commands: mpsc::UnboundedReceiver<Command>,
    classify: Classify,
) {
    // GIO's volume monitors deliver their signals through the process-wide default main context
    // (a private context never sees them), and nothing else in this process runs one, so this
    // thread takes it over and runs it, which is what any GLib program's main thread does.
    let context = glib::MainContext::default();
    let main_loop = glib::MainLoop::new(Some(&context), false);

    let outcome = context.acquire().map(|_owner| {
        let ctx = Ctx {
            monitor: gio::VolumeMonitor::get(),
            classify,
            saved: Rc::default(),
            pending: Rc::default(),
            cancelled: Rc::default(),
            next_request: Rc::default(),
            events: events.clone(),
        };
        let last: Rc<RefCell<Option<Vec<Device>>>> = Rc::default();

        // Take a snapshot and send it if it differs from the previous one.
        let publish = {
            let ctx = ctx.clone();
            let main_loop = main_loop.clone();
            Rc::new(move || {
                let devices = snapshot(&ctx);
                if last.borrow().as_ref() == Some(&devices) {
                    return;
                }
                *last.borrow_mut() = Some(devices.clone());
                if ctx.events.unbounded_send(Event::Devices(devices)).is_err() {
                    main_loop.quit();
                }
            })
        };

        macro_rules! refresh_on {
            ($($connect:ident),+) => {$({
                let publish = publish.clone();
                ctx.monitor.$connect(move |_, _| publish());
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
            let ctx = ctx.clone();
            let publish = publish.clone();
            let main_loop = main_loop.clone();
            glib::spawn_future_local(async move {
                while let Some(command) = commands.next().await {
                    // One task per command: a phone or server that hangs while mounting must not
                    // block unmounting another, and answers to password prompts must get through
                    // while the mount that asked is still waiting.
                    let ctx = ctx.clone();
                    let publish = publish.clone();
                    glib::spawn_future_local(async move {
                        execute(&ctx, command).await;
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
        tracing::error!(
            "cannot run the GLib main context, phones and shares will not be listed: {err}"
        );
    }
}

/// Volumes GVfs serves for phones, and fstab entries of network filesystems.
fn classify_volume(volume: &gio::Volume) -> Option<Kind> {
    if let Some(scheme) = volume.activation_root().and_then(|root| root.uri_scheme()) {
        if is_phone_scheme(&scheme) {
            return Some(Kind::Phone);
        }
        if shares::is_share_scheme(&scheme) {
            return Some(Kind::Share);
        }
    }
    // An fstab entry the user may mount (`user`, `users`): its device column says what it is.
    volume
        .identifier(gio::VOLUME_IDENTIFIER_KIND_UNIX_DEVICE)
        .filter(|device| shares::is_network_device(device))
        .map(|_| Kind::Share)
}

fn is_phone_scheme(scheme: &str) -> bool {
    PHONE_SCHEMES
        .iter()
        .any(|phone| phone.eq_ignore_ascii_case(scheme))
}

/// A mount that is a network share: GVfs's `smb://` and `nfs://`, or a kernel `cifs`/`nfs` mount.
fn is_network_mount(root: &gio::File, kernel_mounts: &[PathBuf]) -> bool {
    match root.uri_scheme().as_deref() {
        Some("file") => root
            .path()
            .is_some_and(|path| kernel_mounts.contains(&path)),
        Some(scheme) => shares::is_share_scheme(scheme),
        None => false,
    }
}

fn device_id(volume: &gio::Volume) -> String {
    volume
        .activation_root()
        .map(|root| root.uri().to_string())
        .or_else(|| {
            volume
                .identifier(gio::VOLUME_IDENTIFIER_KIND_UNIX_DEVICE)
                .map(|device| device.to_string())
        })
        .or_else(|| volume.uuid().map(|uuid| uuid.to_string()))
        .unwrap_or_else(|| volume.name().to_string())
}

fn location_of(root: &gio::File) -> Location {
    root.path()
        .map_or_else(|| Location::Uri(root.uri().to_string()), Location::Path)
}

fn snapshot(ctx: &Ctx) -> Vec<Device> {
    let mounts = ctx.monitor.mounts();
    let mut devices = Vec::new();

    // Phones, and shares from fstab: GIO knows them as volumes, mounted or not.
    for volume in ctx.monitor.volumes() {
        if let Some(kind) = (ctx.classify)(&volume) {
            devices.push(Device {
                id: device_id(&volume),
                name: volume.name().to_string(),
                kind,
                location: volume.get_mount().map(|mount| location_of(&mount.root())),
                saved: false,
            });
        }
    }

    // Shares the user added: listed even when not mounted (GVfs forgets unmounted ones).
    let mut known = HashSet::new();
    for uri in ctx.saved.borrow().iter() {
        let key = shares::key(uri);
        let mount = mounts
            .iter()
            .find(|mount| shares::key(&mount.root().uri()) == key);
        devices.push(Device {
            id: uri.clone(),
            name: shares::display_name(uri),
            kind: Kind::Share,
            location: mount.map(|mount| location_of(&mount.root())),
            saved: true,
        });
        known.insert(key);
    }

    // Shares mounted some other way (the file manager, mount(8)); nothing to remember about them.
    let kernel_mounts = shares::network_mount_points();
    for mount in &mounts {
        let root = mount.root();
        if mount.volume().is_some()
            || !is_network_mount(&root, &kernel_mounts)
            || known.contains(&shares::key(&root.uri()))
        {
            continue;
        }
        devices.push(Device {
            id: root.uri().to_string(),
            name: mount.name().to_string(),
            kind: Kind::Share,
            location: Some(location_of(&root)),
            saved: false,
        });
    }

    // The monitor's order is not guaranteed; keep the list (and the diff against the last one)
    // stable.
    devices.sort_by(|a, b| {
        a.kind
            .cmp(&b.kind)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.id.cmp(&b.id))
    });
    devices
}

enum Target {
    Volume(gio::Volume),
    Mount(gio::Mount),
    /// A share address; it may or may not be saved or mounted yet.
    Share(String),
}

fn find(ctx: &Ctx, id: &str) -> Result<Target, String> {
    if let Some(volume) = ctx
        .monitor
        .volumes()
        .into_iter()
        .find(|volume| (ctx.classify)(volume).is_some() && device_id(volume) == id)
    {
        return Ok(Target::Volume(volume));
    }
    if shares::is_share_uri(id) {
        return Ok(Target::Share(id.to_string()));
    }
    let kernel_mounts = shares::network_mount_points();
    ctx.monitor
        .mounts()
        .into_iter()
        .find(|mount| {
            mount.volume().is_none()
                && is_network_mount(&mount.root(), &kernel_mounts)
                && mount.root().uri() == id
        })
        .map(Target::Mount)
        .ok_or_else(|| "The device is no longer connected".to_string())
}

/// Text for an error; empty when GVfs already told the user (e.g. they cancelled the prompt).
fn describe(err: &glib::Error) -> String {
    if err.matches(gio::IOErrorEnum::FailedHandled) {
        String::new()
    } else {
        err.message().to_string()
    }
}

async fn execute(ctx: &Ctx, command: Command) {
    match command {
        Command::Mount(id, reply) => {
            let _ = reply.send(mount(ctx, &id).await);
        }
        Command::Unmount(id, reply) => {
            let _ = reply.send(unmount(ctx, &id).await);
        }
        Command::SetSaved(saved) => *ctx.saved.borrow_mut() = saved,
        Command::Answer(request, credentials) => ctx.answer(request, credentials),
    }
}

/// Where `root` can be opened, once that place exists. The FUSE path appears a moment after the
/// mount call returns, and opening it too early makes the file manager report a missing folder.
async fn ready_location(root: &gio::File) -> Location {
    let location = location_of(root);
    let Location::Path(path) = &location else {
        return location;
    };
    for _ in 0..MOUNT_SETTLE_TRIES {
        if path.exists() {
            return location;
        }
        glib::timeout_future(MOUNT_SETTLE_STEP).await;
    }
    Location::Uri(root.uri().to_string())
}

async fn mount(ctx: &Ctx, id: &str) -> Result<Location, String> {
    match find(ctx, id)? {
        Target::Volume(volume) => mount_volume(&volume).await,
        Target::Mount(mount) => Ok(ready_location(&mount.root()).await),
        Target::Share(uri) => mount_share(ctx, &uri).await,
    }
}

async fn mount_volume(volume: &gio::Volume) -> Result<Location, String> {
    if volume.get_mount().is_none() {
        match volume
            .mount_future(gio::MountMountFlags::NONE, None::<&gio::MountOperation>)
            .await
        {
            Ok(()) => {}
            Err(err) if err.matches(gio::IOErrorEnum::AlreadyMounted) => {}
            Err(err) => return Err(describe(&err)),
        }
    }

    // The mount call can return before the monitor has processed the new mount.
    for _ in 0..MOUNT_SETTLE_TRIES {
        if let Some(mount) = volume.get_mount() {
            return Ok(ready_location(&mount.root()).await);
        }
        glib::timeout_future(MOUNT_SETTLE_STEP).await;
    }
    // Still not reported: the activation root is where the mount lives anyway.
    match volume.activation_root() {
        Some(root) => Ok(ready_location(&root).await),
        None => Err("The device did not report a location after mounting".to_string()),
    }
}

async fn mount_share(ctx: &Ctx, uri: &str) -> Result<Location, String> {
    let file = gio::File::for_uri(uri);
    let operation = ctx.operation(uri);
    ctx.cancelled.borrow_mut().remove(uri);
    match file
        .mount_enclosing_volume_future(gio::MountMountFlags::NONE, Some(&operation))
        .await
    {
        Ok(()) => {}
        Err(err) if err.matches(gio::IOErrorEnum::AlreadyMounted) => {}
        Err(err) => {
            // Silent when it failed because the user cancelled the password prompt.
            return Err(if ctx.cancelled.borrow_mut().remove(uri) {
                String::new()
            } else {
                describe(&err)
            });
        }
    }

    for _ in 0..MOUNT_SETTLE_TRIES {
        if let Ok(mount) = file.find_enclosing_mount(gio::Cancellable::NONE) {
            return Ok(ready_location(&mount.root()).await);
        }
        glib::timeout_future(MOUNT_SETTLE_STEP).await;
    }
    Ok(ready_location(&file).await)
}

async fn unmount(ctx: &Ctx, id: &str) -> Result<(), String> {
    let mount = match find(ctx, id)? {
        Target::Volume(volume) => volume.get_mount(),
        Target::Mount(mount) => Some(mount),
        Target::Share(uri) => {
            let key = shares::key(&uri);
            ctx.monitor
                .mounts()
                .into_iter()
                .find(|mount| shares::key(&mount.root().uri()) == key)
        }
    };
    let Some(mount) = mount else {
        return Ok(()); // already unmounted
    };
    mount
        .unmount_with_operation_future(gio::MountUnmountFlags::NONE, None::<&gio::MountOperation>)
        .await
        .map_err(|err| describe(&err))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_phone_schemes_count_as_phones() {
        assert!(is_phone_scheme("mtp"));
        assert!(is_phone_scheme("gphoto2"));
        assert!(is_phone_scheme("MTP"));
        for other in ["", "file", "smb", "nfs", "sftp", "afc", "mtpx", "trash"] {
            assert!(!is_phone_scheme(other), "{other:?}");
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
    fn kernel_mounts_count_as_shares_only_when_the_kernel_says_so() {
        let kernel = [PathBuf::from("/mnt/nas")];
        assert!(is_network_mount(&gio::File::for_path("/mnt/nas"), &kernel));
        assert!(!is_network_mount(&gio::File::for_path("/mnt/usb"), &kernel));
        assert!(is_network_mount(
            &gio::File::for_uri("smb://nas/media"),
            &[]
        ));
        assert!(is_network_mount(
            &gio::File::for_uri("nfs://nas/export"),
            &[]
        ));
        assert!(!is_network_mount(&gio::File::for_uri("mtp://phone/"), &[]));
    }

    #[test]
    fn passwords_stay_out_of_debug_output() {
        let credentials = Credentials {
            username: "alice".into(),
            domain: String::new(),
            password: Secret::new("hunter2".into()),
            anonymous: false,
        };
        let shown = format!("{credentials:?}");
        assert!(!shown.contains("hunter2"), "{shown}");
        assert_eq!(credentials.password.expose(), "hunter2");
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
