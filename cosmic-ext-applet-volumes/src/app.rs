// SPDX-License-Identifier: GPL-3.0-only

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

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
    /// Volumes with a mount/unmount in flight.
    busy: HashSet<OwnedObjectPath>,
    /// Last failure, shown at the top of the popup until dismissed.
    error: Option<String>,
    token_tx: Option<calloop::channel::Sender<TokenRequest>>,
    pending_open: Option<PathBuf>,
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
    Open(PathBuf),
    Mount(OwnedObjectPath),
    Remove(OwnedObjectPath),
    Mounted(OwnedObjectPath, Result<PathBuf, String>),
    Removed(OwnedObjectPath, Result<(), String>),
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
                busy: HashSet::new(),
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
            Message::Open(path) => {
                let requested = self.token_tx.as_ref().is_some_and(|tx| {
                    tx.send(TokenRequest {
                        app_id: APP_ID.to_string(),
                        exec: "xdg-open".to_string(),
                    })
                    .is_ok()
                });
                if requested {
                    self.pending_open = Some(path);
                } else {
                    open_in_file_manager(&path, None);
                }
                return self.close_popup();
            }
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
                    Ok(mount_point) => return self.update(Message::Open(mount_point)),
                    Err(reason) => self.fail(&path, &reason),
                }
            }
            Message::Removed(path, result) => {
                if let Err(reason) = result {
                    self.fail(&path, &reason);
                }
                self.busy.remove(&path);
            }
            Message::DismissError => self.error = None,
            Message::Token(update) => match update {
                TokenUpdate::Init(tx) => self.token_tx = Some(tx),
                TokenUpdate::Finished => self.token_tx = None,
                TokenUpdate::ActivationToken { token, .. } => {
                    if let Some(path) = self.pending_open.take() {
                        open_in_file_manager(&path, token);
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

        match &self.state {
            State::Connecting => {
                content = content.push(padded_control(
                    row![indeterminate_circular().size(24.0)].align_y(Alignment::Center),
                ));
            }
            State::Unavailable(reason) => {
                content = content
                    .push(padded_control(text::body(fl!("drive-service-unavailable"))))
                    .push(padded_control(text::caption(reason.as_str())));
            }
            State::Ready if self.volumes.is_empty() => {
                content = content.push(padded_control(
                    text::body(fl!("no-devices-mounted"))
                        .width(Length::Fill)
                        .align_x(Alignment::Center),
                ));
            }
            State::Ready => {
                let rows = self.volumes.iter().map(|v| self.volume_row(v));
                content = content.push(scrollable(Column::with_children(rows)));
            }
        }

        self.core.applet.popup_container(content).into()
    }

    fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            udisks_subscription().map(Message::Watch),
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

    fn volume_row<'a>(&self, volume: &'a Volume) -> Element<'a, Message> {
        let busy = self.busy.contains(&volume.path);

        let mut row = row![
            icon::from_name(volume.kind.icon_name())
                .size(16)
                .symbolic(true),
            text::body(volume.label.as_str()).width(Length::Fill),
        ]
        .align_y(Alignment::Center)
        .spacing(12);

        row = row.push(if volume.mount_point.is_some() {
            text::caption(udisks::human_size(volume.size))
        } else {
            text::caption(fl!("mount"))
        });

        if busy {
            row = row.push(indeterminate_circular().size(24.0));
        } else if volume.mount_point.is_some()
            || udisks::release_plan(&self.volumes, volume) != Release::Nothing
        {
            row = row.push(
                button::icon(icon::from_name("media-eject-symbolic").size(16))
                    .icon_size(16)
                    .on_press(Message::Remove(volume.path.clone())),
            );
        }

        let on_press = match (&volume.mount_point, busy) {
            (_, true) => None,
            (Some(mount_point), false) => Some(Message::Open(mount_point.clone())),
            (None, false) => Some(Message::Mount(volume.path.clone())),
        };

        menu_button(row).on_press_maybe(on_press).into()
    }

    fn fail(&mut self, path: &OwnedObjectPath, reason: &str) {
        let name = self
            .volumes
            .iter()
            .find(|v| &v.path == path)
            .map_or_else(|| path.to_string(), |v| v.label.clone());
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

/// Open a folder with whatever the user has set as their file manager.
///
/// The activation token lets the compositor focus the window that appears.
fn open_in_file_manager(path: &Path, token: Option<String>) {
    let mut command = std::process::Command::new("xdg-open");
    command.arg(path);
    if let Some(token) = token {
        command.env("XDG_ACTIVATION_TOKEN", &token);
        command.env("DESKTOP_STARTUP_ID", &token);
    }
    tokio::spawn(cosmic_launch::process::spawn(command));
}
