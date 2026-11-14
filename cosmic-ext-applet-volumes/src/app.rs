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
    widget::{
        self, button, divider, icon, indeterminate_circular, scrollable, text,
        text_input::{secure_input, text_input},
        toggler,
    },
};
use zbus::OwnedObjectPath;

use crate::{
    fl,
    gvfs::{self, Location},
    shares::{self, AddressError},
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
    /// Phones and network shares (not disks, so not UDisks2's business).
    devices: Vec<gvfs::Device>,
    gvfs: Option<gvfs::Handle>,
    /// Shares the user added with "Connect to server".
    saved: Vec<String>,
    /// GVfs is waiting for a user name and password.
    credentials: Option<CredentialForm>,
    /// The "Connect to server" address field, `Some` while it is open.
    connect: Option<String>,
    /// Volumes with a mount/unmount in flight.
    busy: HashSet<OwnedObjectPath>,
    /// Same for devices, by their GIO id.
    busy_devices: HashSet<String>,
    /// Last failure, shown at the top of the popup until dismissed.
    error: Option<String>,
    token_tx: Option<calloop::channel::Sender<TokenRequest>>,
    pending_open: Option<Location>,
}

struct CredentialForm {
    request: gvfs::CredentialRequest,
    username: String,
    domain: String,
    password: gvfs::Secret,
    anonymous: bool,
    show_password: bool,
}

impl CredentialForm {
    fn new(request: gvfs::CredentialRequest) -> Self {
        // SMB's placeholder user name is not something to prefill.
        let username = match request.default_user.as_str() {
            "guest" => String::new(),
            user => user.to_string(),
        };
        Self {
            username,
            domain: request.default_domain.clone(),
            password: gvfs::Secret::default(),
            anonymous: false,
            show_password: false,
            request,
        }
    }
}

#[derive(Debug, Clone)]
pub enum CredentialField {
    Username(String),
    Domain(String),
    Password(gvfs::Secret),
    Anonymous(bool),
    ToggleShowPassword,
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
    Gvfs(gvfs::Event),
    Open(Location),
    Mount(OwnedObjectPath),
    Remove(OwnedObjectPath),
    Mounted(OwnedObjectPath, Result<std::path::PathBuf, String>),
    Removed(OwnedObjectPath, Result<(), String>),
    MountDevice(String),
    RemoveDevice(String),
    DeviceMounted(String, Result<Location, String>),
    DeviceRemoved(String, Result<(), String>),
    CredentialEdit(CredentialField),
    CredentialSubmit,
    CredentialCancel,
    ConnectToggle,
    ConnectInput(String),
    ConnectSubmit,
    ForgetShare(String),
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
                gvfs: None,
                saved: shares::load(),
                credentials: None,
                connect: None,
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
            Message::Gvfs(gvfs::Event::Ready(handle)) => {
                handle.set_saved(self.saved.clone());
                self.gvfs = Some(handle);
            }
            Message::Gvfs(gvfs::Event::Devices(devices)) => self.devices = devices,
            Message::Gvfs(gvfs::Event::Credentials(request)) => {
                self.credentials = Some(CredentialForm::new(request));
            }
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
            Message::CredentialEdit(_)
            | Message::CredentialSubmit
            | Message::CredentialCancel
            | Message::ConnectToggle
            | Message::ConnectInput(_)
            | Message::ConnectSubmit
            | Message::ForgetShare(_) => return self.update_shares(message),
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

        if let Some(form) = &self.credentials {
            content = content
                .push(Self::credentials_view(form))
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
        } else if matches!(self.state, State::Ready) && self.busy_devices.is_empty() {
            content = content.push(padded_control(
                text::body(fl!("no-devices-mounted"))
                    .width(Length::Fill)
                    .align_x(Alignment::Center),
            ));
        }

        // Shares being connected for the first time have no row yet.
        for id in &self.busy_devices {
            if !self.devices.iter().any(|d| &d.id == id) {
                content = content.push(padded_control(
                    row![
                        indeterminate_circular().size(24.0),
                        text::body(shares::display_name(id)),
                    ]
                    .spacing(space_s)
                    .align_y(Alignment::Center),
                ));
            }
        }

        content = content
            .push(padded_control(divider::horizontal::default()).padding([space_xxs, space_s]))
            .push(self.connect_view());

        self.core.applet.popup_container(content).into()
    }

    fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            udisks_subscription().map(Message::Watch),
            gvfs_subscription().map(Message::Gvfs),
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
                let Some(handle) = self.gvfs.clone() else {
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
                let Some(handle) = self.gvfs.clone() else {
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
                    Ok(location) => {
                        self.remember_share(&id);
                        return self.open(location);
                    }
                    // The user cancelled a password prompt: nothing to report.
                    Err(reason) if reason.is_empty() => {}
                    Err(reason) => {
                        let hint = match self.devices.iter().find(|d| d.id == id).map(|d| d.kind) {
                            // Nearly always: the phone is locked, or its USB mode is "charging only".
                            Some(gvfs::Kind::Phone) => fl!("phone-mount-hint"),
                            _ => fl!("share-mount-hint"),
                        };
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

    /// "Connect to server", saved shares and the password prompt.
    fn update_shares(&mut self, message: Message) -> app::Task<Message> {
        match message {
            Message::CredentialEdit(field) => {
                if let Some(form) = &mut self.credentials {
                    match field {
                        CredentialField::Username(text) => form.username = text,
                        CredentialField::Domain(text) => form.domain = text,
                        CredentialField::Password(secret) => form.password = secret,
                        CredentialField::Anonymous(on) => form.anonymous = on,
                        CredentialField::ToggleShowPassword => form.show_password ^= true,
                    }
                }
            }
            Message::CredentialSubmit => self.answer_credentials(true),
            Message::CredentialCancel => self.answer_credentials(false),
            Message::ConnectToggle => {
                if self.connect.take().is_none() {
                    self.connect = Some(String::new());
                    return widget::text_input::focus::<cosmic::Action<Message>>(connect_id());
                }
            }
            Message::ConnectInput(text) => self.connect = Some(text),
            Message::ConnectSubmit => return self.connect_to_server(),
            Message::ForgetShare(id) => {
                self.saved.retain(|share| share != &id);
                self.persist_saved();
            }
            _ => {}
        }
        Task::none()
    }

    fn answer_credentials(&mut self, submit: bool) {
        let Some(form) = self.credentials.take() else {
            return;
        };
        if let Some(handle) = &self.gvfs {
            let credentials = submit.then_some(gvfs::Credentials {
                username: form.username,
                domain: form.domain,
                password: form.password,
                anonymous: form.anonymous,
            });
            handle.answer(form.request.request, credentials);
        }
    }

    fn connect_to_server(&mut self) -> app::Task<Message> {
        let Some(input) = self.connect.clone() else {
            return Task::none();
        };
        match shares::normalize(&input) {
            Ok(address) => {
                self.connect = None;
                self.update_device(Message::MountDevice(address))
            }
            Err(err) => {
                self.error = Some(match err {
                    AddressError::Invalid => fl!("invalid-address"),
                    AddressError::PasswordInAddress => fl!("password-in-address"),
                });
                Task::none()
            }
        }
    }

    /// A share typed into "Connect to server" is saved once it has mounted successfully.
    fn remember_share(&mut self, id: &str) {
        let typed = shares::is_share_uri(id)
            && !self.devices.iter().any(|d| d.id == id)
            && !self.saved.iter().any(|s| shares::key(s) == shares::key(id));
        if typed {
            self.saved.push(id.to_string());
            self.persist_saved();
        }
    }

    fn persist_saved(&self) {
        if let Err(err) = shares::save(&self.saved) {
            tracing::warn!("cannot save the list of network shares: {err}");
        }
        if let Some(handle) = &self.gvfs {
            handle.set_saved(self.saved.clone());
        }
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
            action: can_remove
                .then(|| ("media-eject-symbolic", Message::Remove(volume.path.clone()))),
        })
    }

    fn device_row<'a>(&self, device: &'a gvfs::Device) -> Element<'a, Message> {
        row_view(RowParts {
            icon: match device.kind {
                gvfs::Kind::Phone => "phone-symbolic",
                gvfs::Kind::Share => "folder-remote-symbolic",
            },
            label: device.name.as_str(),
            caption: device.location.is_none().then(|| fl!("mount")),
            busy: self.busy_devices.contains(&device.id),
            press: match &device.location {
                Some(location) => Message::Open(location.clone()),
                None => Message::MountDevice(device.id.clone()),
            },
            action: if device.location.is_some() {
                Some((
                    "media-eject-symbolic",
                    Message::RemoveDevice(device.id.clone()),
                ))
            } else if device.saved {
                // Not mounted and added by the user: the only thing to do besides mounting.
                Some((
                    "edit-delete-symbolic",
                    Message::ForgetShare(device.id.clone()),
                ))
            } else {
                None
            },
        })
    }

    /// The user name and password GVfs asked for.
    fn credentials_view(form: &CredentialForm) -> Element<'_, Message> {
        let Spacing {
            space_xxs, space_s, ..
        } = theme::active().cosmic().spacing;
        let (heading, detail) = form
            .request
            .message
            .split_once('\n')
            .unwrap_or((form.request.message.as_str(), ""));

        let mut fields = column![text::heading(heading), text::body(detail)].spacing(space_xxs);
        if form.request.anonymous {
            fields = fields.push(
                toggler(form.anonymous)
                    .label(fl!("connect-anonymously"))
                    .on_toggle(|on| Message::CredentialEdit(CredentialField::Anonymous(on))),
            );
        }
        if !form.anonymous {
            if form.request.username {
                fields = fields.push(
                    text_input(fl!("username"), &form.username)
                        .on_input(|text| Message::CredentialEdit(CredentialField::Username(text))),
                );
            }
            if form.request.domain {
                fields = fields.push(
                    text_input(fl!("domain"), &form.domain)
                        .on_input(|text| Message::CredentialEdit(CredentialField::Domain(text))),
                );
            }
            if form.request.password {
                fields = fields.push(
                    secure_input(
                        fl!("password"),
                        form.password.expose(),
                        Some(Message::CredentialEdit(CredentialField::ToggleShowPassword)),
                        !form.show_password,
                    )
                    .on_input(|text| {
                        Message::CredentialEdit(CredentialField::Password(gvfs::Secret::new(text)))
                    })
                    .on_submit(|_| Message::CredentialSubmit),
                );
            }
        }
        fields = fields.push(
            row![
                button::standard(fl!("cancel")).on_press(Message::CredentialCancel),
                button::suggested(fl!("connect")).on_press(Message::CredentialSubmit),
            ]
            .spacing(space_s),
        );
        padded_control(fields).into()
    }

    /// "Connect to server…" and, once opened, its address field.
    fn connect_view(&self) -> Element<'_, Message> {
        let Spacing {
            space_xxs, space_s, ..
        } = theme::active().cosmic().spacing;
        match &self.connect {
            None => menu_button(
                row![
                    icon::from_name("list-add-symbolic").size(16).symbolic(true),
                    text::body(fl!("connect-to-server")).width(Length::Fill),
                ]
                .spacing(12)
                .align_y(Alignment::Center),
            )
            .on_press(Message::ConnectToggle)
            .into(),
            Some(address) => padded_control(
                column![
                    text_input(fl!("server-placeholder"), address)
                        .id(connect_id())
                        .on_input(Message::ConnectInput)
                        .on_submit(|_| Message::ConnectSubmit),
                    row![
                        button::standard(fl!("cancel")).on_press(Message::ConnectToggle),
                        button::suggested(fl!("connect")).on_press(Message::ConnectSubmit),
                    ]
                    .spacing(space_s),
                ]
                .spacing(space_xxs),
            )
            .into(),
        }
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

fn gvfs_subscription() -> Subscription<gvfs::Event> {
    Subscription::run_with("cosmic-ext-applet-volumes/gvfs", |_| {
        stream::channel(8, gvfs::run)
    })
}

fn connect_id() -> widget::Id {
    widget::Id::new("cosmic-ext-applet-volumes-connect")
}

/// What a row in the popup shows and does; drives and phones share the same look.
struct RowParts<'a> {
    icon: &'static str,
    label: &'a str,
    caption: Option<String>,
    busy: bool,
    press: Message,
    /// A button at the end of the row: its icon and what it does.
    action: Option<(&'static str, Message)>,
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
    } else if let Some((icon_name, message)) = parts.action {
        row = row.push(
            button::icon(icon::from_name(icon_name).size(16))
                .icon_size(16)
                .on_press(message),
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
