// SPDX-License-Identifier: GPL-3.0-only

use std::collections::HashSet;

use cosmic::{
    Element, Task, app,
    applet::{
        menu_button, padded_control,
        token::subscription::{TokenRequest, TokenUpdate, activation_token_subscription},
    },
    cctk::sctk::reexports::calloop,
    cosmic_theme::Spacing,
    iced::{
        Alignment, Length, Limits, Subscription, stream,
        widget::{Column, column, row},
        window,
    },
    surface::action::LiveSettings,
    theme,
    widget::{button, divider, icon, indeterminate_circular, scrollable, text},
};
use zbus::OwnedObjectPath;

use crate::{
    fl,
    mtp::{self, Location},
    udisks::{self, Release, Volume},
    watch::{self, Event},
};

const APP_ID: &str = "local.cosmicext.Volumes";
const PANEL_ICON: &str = "media-eject-symbolic";

pub struct VolumesApplet {
    core: cosmic::app::Core,
    popup: Option<window::Id>,
    state: State,
    volumes: Vec<Volume>,
    /// Phones and other MTP devices (not disks, so not UDisks2's business).
    devices: Vec<mtp::Device>,
    mtp: Option<mtp::Handle>,
    /// Volumes with a mount/unmount in flight.
    busy: HashSet<OwnedObjectPath>,
    /// Same for devices, by their GIO id.
    busy_devices: HashSet<String>,
    /// Last failure, shown at the top of the popup until dismissed.
    error: Option<String>,
    token_tx: Option<calloop::channel::Sender<TokenRequest>>,
    pending_open: Option<Location>,
}

enum State {
    Connecting,
    Ready,
    Unavailable(String),
}

#[derive(Debug, Clone)]
pub enum Message {
    TogglePopup,
    PopupClosed(window::Id),
    Watch(Event),
    Mtp(mtp::Event),
    Open(Location),
    Mount(OwnedObjectPath),
    Remove(OwnedObjectPath),
    Mounted(OwnedObjectPath, Result<std::path::PathBuf, String>),
    Removed(OwnedObjectPath, Result<(), String>),
    MountDevice(String),
    RemoveDevice(String),
    DeviceMounted(String, Result<Location, String>),
    DeviceRemoved(String, Result<(), String>),
    DismissError,
    Token(TokenUpdate),
}

impl cosmic::Application for VolumesApplet {
    type Executor = cosmic::SingleThreadExecutor;
    type Flags = ();
    type Message = Message;
    const APP_ID: &'static str = APP_ID;

    fn core(&self) -> &cosmic::app::Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut cosmic::app::Core {
        &mut self.core
    }

    fn init(core: cosmic::app::Core, _flags: ()) -> (Self, app::Task<Self::Message>) {
        (
            Self {
                core,
                popup: None,
                state: State::Connecting,
                volumes: Vec::new(),
                devices: Vec::new(),
                mtp: None,
                busy: HashSet::new(),
                busy_devices: HashSet::new(),
                error: None,
                token_tx: None,
                pending_open: None,
            },
            Task::none(),
        )
    }

    fn on_close_requested(&self, id: window::Id) -> Option<Message> {
        Some(Message::PopupClosed(id))
    }

    fn update(&mut self, message: Self::Message) -> app::Task<Self::Message> {
        match message {
            Message::TogglePopup => return self.toggle_popup(),
            Message::PopupClosed(id) => {
                if self.popup == Some(id) {
                    self.popup = None;
                }
            }
            Message::Watch(Event::Volumes(volumes)) => {
                self.volumes = volumes;
                self.state = State::Ready;
            }
            Message::Watch(Event::Unavailable(reason)) => {
                self.volumes.clear();
                self.state = State::Unavailable(reason);
            }
            Message::Mtp(mtp::Event::Ready(handle)) => self.mtp = Some(handle),
            Message::Mtp(mtp::Event::Devices(devices)) => self.devices = devices,
            Message::Open(location) => return self.open(location),
            Message::Mount(path) => {
                self.error = None;
                self.busy.insert(path.clone());
                return Task::perform(udisks::mount(path.clone()), move |result| {
                    cosmic::action::app(Message::Mounted(path.clone(), result))
                });
            }
            Message::Remove(path) => {
                let Some(volume) = self.volumes.iter().find(|v| v.path == path) else {
                    return Task::none();
                };
                self.error = None;
                let mounted = volume.mount_point.is_some();
                let release = udisks::release_plan(&self.volumes, volume);
                self.busy.insert(path.clone());
                return Task::perform(
                    udisks::remove(path.clone(), mounted, release),
                    move |result| cosmic::action::app(Message::Removed(path.clone(), result)),
                );
            }
            Message::Mounted(path, result) => {
                self.busy.remove(&path);
                match result {
                    Ok(mount_point) => {
                        return self.open(Location::Path(mount_point));
                    }
                    Err(reason) => self.fail(&path, &reason),
                }
            }
            Message::Removed(path, result) => {
                if let Err(reason) = result {
                    self.fail(&path, &reason);
                }
                self.busy.remove(&path);
            }
            Message::MountDevice(_)
            | Message::RemoveDevice(_)
            | Message::DeviceMounted(..)
            | Message::DeviceRemoved(..) => return self.update_device(message),
            Message::DismissError => self.error = None,
            Message::Token(update) => match update {
                TokenUpdate::Init(tx) => self.token_tx = Some(tx),
                TokenUpdate::Finished => self.token_tx = None,
                TokenUpdate::ActivationToken { token, .. } => {
                    if let Some(location) = self.pending_open.take() {
                        open_in_file_manager(&location, token);
                    }
                }
            },
        }
        Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        self.core
            .applet
            .icon_button(PANEL_ICON)
            .on_press_down(Message::TogglePopup)
            .into()
    }

    fn view_window(&self, id: window::Id) -> Element<'_, Message> {
        let Spacing {
            space_xxs, space_s, ..
        } = theme::active().cosmic().spacing;

        if self.popup != Some(id) {
            return text("").into();
        }

        let mut content = column![].align_x(Alignment::Start).padding([8, 0]);

        if let Some(error) = &self.error {
            content = content
                .push(padded_control(
                    row![
                        icon::from_name("dialog-warning-symbolic")
                            .size(16)
                            .symbolic(true),
                        text::body(error.as_str()).width(Length::Fill),
                        button::icon(icon::from_name("window-close-symbolic").size(16))
                            .icon_size(16)
                            .on_press(Message::DismissError),
                    ]
                    .align_y(Alignment::Center)
                    .spacing(space_s),
                ))
                .push(padded_control(divider::horizontal::default()).padding([space_xxs, space_s]));
        }

        let has_rows = !self.volumes.is_empty() || !self.devices.is_empty();

        match &self.state {
            State::Connecting if !has_rows => {
                content = content.push(padded_control(
                    row![indeterminate_circular().size(24.0)].align_y(Alignment::Center),
                ));
            }
            State::Connecting | State::Ready => {}
            // Phones come from a different service, so they can still be listed here.
            State::Unavailable(reason) => {
                content = content
                    .push(padded_control(text::body(fl!("drive-service-unavailable"))))
                    .push(padded_control(text::caption(reason.as_str())));
            }
        }

        if has_rows {
            let rows = self
                .volumes
                .iter()
                .map(|v| self.volume_row(v))
                .chain(self.devices.iter().map(|d| self.device_row(d)));
            content = content.push(scrollable(Column::with_children(rows)));
        } else if matches!(self.state, State::Ready) {
            content = content.push(padded_control(
                text::body(fl!("no-devices-mounted"))
                    .width(Length::Fill)
                    .align_x(Alignment::Center),
            ));
        }

        self.core.applet.popup_container(content).into()
    }

    fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            udisks_subscription().map(Message::Watch),
            mtp_subscription().map(Message::Mtp),
            activation_token_subscription(0).map(Message::Token),
        ])
    }

    fn style(&self) -> Option<cosmic::iced::theme::Style> {
        Some(cosmic::applet::style())
    }
}

impl VolumesApplet {
    fn toggle_popup(&mut self) -> app::Task<Message> {
        if let Some(p) = self.popup.take() {
            return cosmic::surface::surface_task(cosmic::surface::action::destroy_popup(p));
        }
        cosmic::surface::surface_task(cosmic::surface::action::app_popup(
            |_| LiveSettings::default(),
            |app: &mut VolumesApplet| {
                let new_id = window::Id::unique();
                app.popup.replace(new_id);
                let mut popup_settings = app.core.applet.get_popup_settings(
                    app.core.main_window_id().unwrap(),
                    new_id,
                    None,
                    None,
                    None,
                );
                popup_settings.positioner.size_limits = Limits::NONE
                    .min_width(300.0)
                    .max_width(372.0)
                    .min_height(1.0)
                    .max_height(800.0);
                popup_settings
            },
            None,
        ))
    }

    /// Open a mounted location in the file manager (asking the compositor for a focus token
    /// first when it supports that) and get out of the way.
    fn open(&mut self, location: Location) -> app::Task<Message> {
        let requested = self.token_tx.as_ref().is_some_and(|tx| {
            tx.send(TokenRequest {
                app_id: APP_ID.to_string(),
                exec: launcher(&location).to_string(),
            })
            .is_ok()
        });
        if requested {
            self.pending_open = Some(location);
        } else {
            open_in_file_manager(&location, None);
        }
        self.close_popup()
    }

    /// The phone half of `update`.
    fn update_device(&mut self, message: Message) -> app::Task<Message> {
        match message {
            Message::MountDevice(id) => {
                let Some(handle) = self.mtp.clone() else {
                    return Task::none();
                };
                self.error = None;
                self.busy_devices.insert(id.clone());
                return Task::perform(
                    {
                        let id = id.clone();
                        async move { handle.mount(id).await }
                    },
                    move |result| cosmic::action::app(Message::DeviceMounted(id.clone(), result)),
                );
            }
            Message::RemoveDevice(id) => {
                let Some(handle) = self.mtp.clone() else {
                    return Task::none();
                };
                self.error = None;
                self.busy_devices.insert(id.clone());
                return Task::perform(
                    {
                        let id = id.clone();
                        async move { handle.unmount(id).await }
                    },
                    move |result| cosmic::action::app(Message::DeviceRemoved(id.clone(), result)),
                );
            }
            Message::DeviceMounted(id, result) => {
                self.busy_devices.remove(&id);
                match result {
                    Ok(location) => return self.open(location),
                    Err(reason) => {
                        // Nearly always: the phone is locked, or its USB mode is "charging only".
                        let hint = fl!("phone-mount-hint");
                        self.fail_device(&id, &format!("{reason}. {hint}"));
                    }
                }
            }
            Message::DeviceRemoved(id, result) => {
                self.busy_devices.remove(&id);
                if let Err(reason) = result {
                    self.fail_device(&id, &reason);
                }
            }
            _ => {}
        }
        Task::none()
    }

    fn volume_row<'a>(&self, volume: &'a Volume) -> Element<'a, Message> {
        let mounted = volume.mount_point.is_some();
        let can_remove = mounted || udisks::release_plan(&self.volumes, volume) != Release::Nothing;

        row_view(RowParts {
            icon: volume.kind.icon_name(),
            label: volume.label.as_str(),
            caption: Some(if mounted {
                udisks::human_size(volume.size)
            } else {
                fl!("mount")
            }),
            busy: self.busy.contains(&volume.path),
            press: match &volume.mount_point {
                Some(mount_point) => Message::Open(Location::Path(mount_point.clone())),
                None => Message::Mount(volume.path.clone()),
            },
            eject: can_remove.then(|| Message::Remove(volume.path.clone())),
        })
    }

    fn device_row<'a>(&self, device: &'a mtp::Device) -> Element<'a, Message> {
        row_view(RowParts {
            icon: "phone-symbolic",
            label: device.name.as_str(),
            caption: device.location.is_none().then(|| fl!("mount")),
            busy: self.busy_devices.contains(&device.id),
            press: match &device.location {
                Some(location) => Message::Open(location.clone()),
                None => Message::MountDevice(device.id.clone()),
            },
            eject: device
                .location
                .is_some()
                .then(|| Message::RemoveDevice(device.id.clone())),
        })
    }

    fn fail(&mut self, path: &OwnedObjectPath, reason: &str) {
        let name = self
            .volumes
            .iter()
            .find(|v| &v.path == path)
            .map_or_else(|| path.to_string(), |v| v.label.clone());
        self.error = Some(format!("{name}: {reason}"));
    }

    fn fail_device(&mut self, id: &str, reason: &str) {
        let name = self
            .devices
            .iter()
            .find(|d| d.id == id)
            .map_or_else(|| id.to_string(), |d| d.name.clone());
        self.error = Some(format!("{name}: {reason}"));
    }

    fn close_popup(&mut self) -> app::Task<Message> {
        match self.popup.take() {
            Some(p) => cosmic::surface::surface_task(cosmic::surface::action::destroy_popup(p)),
            None => Task::none(),
        }
    }
}

fn udisks_subscription() -> Subscription<Event> {
    Subscription::run_with("cosmic-ext-applet-volumes/udisks", |_| {
        stream::channel(8, watch::run)
    })
}

fn mtp_subscription() -> Subscription<mtp::Event> {
    Subscription::run_with("cosmic-ext-applet-volumes/mtp", |_| {
        stream::channel(8, mtp::run)
    })
}

/// What a row in the popup shows and does; drives and phones share the same look.
struct RowParts<'a> {
    icon: &'static str,
    label: &'a str,
    caption: Option<String>,
    busy: bool,
    press: Message,
    eject: Option<Message>,
}

fn row_view(parts: RowParts<'_>) -> Element<'_, Message> {
    let mut row = row![
        icon::from_name(parts.icon).size(16).symbolic(true),
        text::body(parts.label).width(Length::Fill),
    ]
    .align_y(Alignment::Center)
    .spacing(12);

    if let Some(caption) = parts.caption {
        row = row.push(text::caption(caption));
    }

    if parts.busy {
        row = row.push(indeterminate_circular().size(24.0));
    } else if let Some(eject) = parts.eject {
        row = row.push(
            button::icon(icon::from_name("media-eject-symbolic").size(16))
                .icon_size(16)
                .on_press(eject),
        );
    }

    menu_button(row)
        .on_press_maybe((!parts.busy).then_some(parts.press))
        .into()
}

/// The program that opens a location; also what the activation token is requested for.
fn launcher(location: &Location) -> &'static str {
    match location {
        Location::Path(_) => "xdg-open",
        Location::Uri(_) => "gio",
    }
}

/// Open a folder with whatever the user has set as their file manager.
///
/// The activation token lets the compositor focus the window that appears.
fn open_in_file_manager(location: &Location, token: Option<String>) {
    let mut command = std::process::Command::new(launcher(location));
    match location {
        Location::Path(path) => {
            command.arg(path);
        }
        // Only reachable through GIO (GVfs's FUSE mount is not available): let `gio` pick the app.
        Location::Uri(uri) => {
            command.arg("open").arg(uri);
        }
    }
    if let Some(token) = token {
        command.env("XDG_ACTIVATION_TOKEN", &token);
        command.env("DESKTOP_STARTUP_ID", &token);
    }
    tokio::spawn(cosmic_launch::process::spawn(command));
}
