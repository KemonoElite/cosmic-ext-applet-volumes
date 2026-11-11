// SPDX-License-Identifier: GPL-3.0-only

//! Live view of the drives UDisks2 knows about.
//!
//! The original applet re-read `/proc/mounts` and enumerated udev from inside `view`, i.e.
//! on every redraw and on the UI thread. This instead keeps one subscription open: it takes
//! a snapshot, then waits for UDisks2 to announce a change (a device or interface appearing
//! or disappearing, mount points changing, the daemon restarting) and takes a new one.

use std::{num::NonZeroUsize, time::Duration};

use futures::{SinkExt, StreamExt, channel::mpsc::Sender};
use zbus::{Connection, MatchRule, MessageStream, message::Type};

use crate::udisks::{self, Volume};

/// Signals tend to arrive in bursts (one plug-in is dozens of property changes).
const DEBOUNCE: Duration = Duration::from_millis(150);
const RETRY_DELAY: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
pub enum Event {
    Volumes(Vec<Volume>),
    Unavailable(String),
}

/// Keep the volume list up to date for as long as the receiver is alive, reconnecting if the
/// bus or the UDisks2 service goes away.
pub async fn run(mut output: Sender<Event>) {
    loop {
        match watch(&mut output).await {
            Ok(()) if output.is_closed() => return,
            Ok(()) => tracing::warn!("UDisks2 signal stream ended, reconnecting"),
            Err(err) => {
                let reason = udisks::describe(&err);
                tracing::warn!("UDisks2 unavailable: {reason}");
                if output.send(Event::Unavailable(reason)).await.is_err() {
                    return;
                }
            }
        }
        tokio::time::sleep(RETRY_DELAY).await;
    }
}

/// Runs until the bus connection goes away or the receiver is dropped.
async fn watch(output: &mut Sender<Event>) -> zbus::Result<()> {
    let connection = Connection::system().await?;

    // Subscribe before the first snapshot so nothing that happens in between is missed.
    let from_udisks = MatchRule::builder()
        .msg_type(Type::Signal)
        .sender(udisks::SERVICE)
        .path_namespace(udisks::ROOT)
        .build()?;
    let owner_changed = MatchRule::builder()
        .msg_type(Type::Signal)
        .sender("org.freedesktop.DBus")
        .interface("org.freedesktop.DBus")
        .member("NameOwnerChanged")
        .add_arg(udisks::SERVICE)
        .build()?;
    let mut changes = futures::stream::select(
        MessageStream::for_match_rule(from_udisks, &connection, NonZeroUsize::new(128))
            .await?
            .map(|_| ()),
        MessageStream::for_match_rule(owner_changed, &connection, NonZeroUsize::new(8))
            .await?
            .map(|_| ()),
    );

    let mut last: Option<Vec<Volume>> = None;
    loop {
        let volumes = udisks::volumes(&udisks::fetch_objects(&connection).await?);
        if last.as_ref() != Some(&volumes) {
            if output.send(Event::Volumes(volumes.clone())).await.is_err() {
                return Ok(());
            }
            last = Some(volumes);
        }

        // Block until something changes, then let the burst settle.
        if changes.next().await.is_none() {
            return Ok(());
        }
        while let Ok(next) = tokio::time::timeout(DEBOUNCE, changes.next()).await {
            if next.is_none() {
                return Ok(());
            }
        }
    }
}
