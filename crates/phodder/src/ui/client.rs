//! The window's connection to the service. Commands go out as calls without
//! replies, so they keep their order. Updates and command results come back
//! as method replies: Flatpak's D-Bus proxy can drop signals sent from one
//! instance of an app to another, but always passes replies.

use super::UiUpdate;
use crate::service::commands::{Reply, ServiceCommand};
use crate::service::{BUS_NAME, CALLS_INTERFACE, INTERFACE, OBJECT_PATH};
use futures_util::StreamExt;
use zbus::proxy::MethodFlags;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

/// Longer than the service's `POLL_WAIT`, so a call that times out here
/// means the service didn't answer.
const UPDATES_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

static CLIENT: OnceLock<Arc<ServiceClient>> = OnceLock::new();

/// Set by Quit, so the service that was asked to stop isn't started again.
static QUITTING: AtomicBool = AtomicBool::new(false);

pub struct ServiceClient {
    connection: zbus::Connection,
}

impl ServiceClient {
    pub async fn start(update_tx: async_channel::Sender<UiUpdate>) -> anyhow::Result<Arc<Self>> {
        let connection = zbus::Connection::session().await?;
        let client = Arc::new(Self { connection });
        let _ = CLIENT.set(client.clone());

        let dbus = zbus::fdo::DBusProxy::new(&client.connection).await?;
        let mut owner_changes = dbus.receive_name_owner_changed_with_args(&[(0, BUS_NAME)]).await?;
        {
            let client = client.clone();
            tokio::spawn(async move {
                while let Some(change) = owner_changes.next().await {
                    let Ok(args) = change.args() else { continue };
                    if args.new_owner().is_none() && !QUITTING.load(Ordering::SeqCst) {
                        tracing::warn!("the Phodder service went away; starting it again");
                        if let Err(e) = client.ensure_service().await {
                            tracing::error!("{e:#}");
                        }
                    }
                }
            });
        }

        client.ensure_service().await?;
        tokio::spawn(client.clone().receive_updates(update_tx));
        Ok(client)
    }

    /// The first call registers this window with the service, which then
    /// sends the current player and feed state.
    async fn receive_updates(self: Arc<Self>, update_tx: async_channel::Sender<UiUpdate>) {
        loop {
            let call = self.connection.call_method(Some(BUS_NAME), OBJECT_PATH, Some(CALLS_INTERFACE), "NextUpdates", &());
            let updates = match tokio::time::timeout(UPDATES_CALL_TIMEOUT, call).await {
                Ok(Ok(reply)) => reply.body().deserialize::<Vec<String>>().unwrap_or_default(),
                Ok(Err(e)) => {
                    tracing::warn!("couldn't get updates from the Phodder service: {e}");
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    continue;
                }
                Err(_) => {
                    tracing::warn!("the Phodder service didn't answer a request for updates");
                    continue;
                }
            };
            for json in updates {
                match serde_json::from_str::<UiUpdate>(&json) {
                    Ok(update) => {
                        if update_tx.send(update).await.is_err() {
                            return;
                        }
                    }
                    Err(e) => tracing::warn!("ignoring an update this window can't read: {e}"),
                }
            }
        }
    }

    /// D-Bus activation first, which is the only way that works inside
    /// Flatpak, then starting the binary directly.
    async fn ensure_service(&self) -> anyhow::Result<()> {
        let dbus = zbus::fdo::DBusProxy::new(&self.connection).await?;
        if dbus.name_has_owner(BUS_NAME.try_into()?).await? {
            return Ok(());
        }
        match dbus.start_service_by_name(BUS_NAME.try_into()?, 0).await {
            Ok(_) => return Ok(()),
            Err(e) => tracing::info!("D-Bus couldn't start the service ({e}); starting it directly"),
        }
        crate::service::spawn_self(&["--service"])?;
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if dbus.name_has_owner(BUS_NAME.try_into()?).await? {
                return Ok(());
            }
        }
        anyhow::bail!("the Phodder service didn't start")
    }

    /// Commands without a reply are sent without waiting, so they go out in
    /// order and a burst of seeks doesn't wait a round trip each. Ones with a
    /// reply wait for it in their own task.
    pub async fn send(self: &Arc<Self>, command: ServiceCommand, reply: Option<Reply>) {
        let json = match serde_json::to_string(&command) {
            Ok(json) => json,
            Err(e) => {
                tracing::error!("couldn't encode a command: {e}");
                return;
            }
        };
        let Some(reply) = reply else {
            let result = async {
                let message = zbus::Message::method(OBJECT_PATH, "Command")?
                    .destination(BUS_NAME)?
                    .interface(INTERFACE)?
                    .with_flags(zbus::message::Flags::NoReplyExpected)?
                    .build(&(json,))?;
                self.connection.send(&message).await
            }
            .await;
            if let Err(e) = result {
                tracing::error!("couldn't send a command to the Phodder service: {e}");
            }
            return;
        };
        let client = self.clone();
        tokio::spawn(async move {
            let result = match client.connection.call_method(Some(BUS_NAME), OBJECT_PATH, Some(CALLS_INTERFACE), "Request", &(json,)).await {
                Ok(message) => message
                    .body()
                    .deserialize::<String>()
                    .map_err(|e| e.to_string())
                    .and_then(|json| serde_json::from_str::<Result<(), String>>(&json).map_err(|e| e.to_string()))
                    .and_then(|result| result),
                Err(e) => Err(e.to_string()),
            };
            let _ = reply.send(result);
        });
    }
}

/// Tells the service this window is closing, so it can stop at once when
/// Run in Background is off. Called on the GTK thread as the window quits.
pub fn say_goodbye(tokio_handle: &tokio::runtime::Handle) {
    if QUITTING.load(Ordering::SeqCst) {
        return;
    }
    let Some(client) = CLIENT.get() else { return };
    // A call to a stopped service would start it through D-Bus activation.
    let call = call_without_starting(&client.connection, "Goodbye", false, &());
    match tokio_handle.block_on(async { tokio::time::timeout(std::time::Duration::from_secs(1), call).await }) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!("couldn't say goodbye to the Phodder service: {e}"),
        Err(_) => tracing::warn!("the Phodder service didn't answer the window's goodbye"),
    }
}

/// Stops the service along with the window. Called on the GTK thread by
/// Quit; it waits until the message is written so it isn't lost when the
/// process exits.
pub fn quit_service(tokio_handle: &tokio::runtime::Handle) {
    QUITTING.store(true, Ordering::SeqCst);
    let Some(client) = CLIENT.get() else { return };
    let Ok(json) = serde_json::to_string(&ServiceCommand::Quit) else { return };
    let body = (json,);
    let send = call_without_starting(&client.connection, "Command", true, &body);
    match tokio_handle.block_on(async { tokio::time::timeout(std::time::Duration::from_secs(1), send).await }) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!("couldn't ask the Phodder service to quit: {e}"),
        Err(_) => tracing::warn!("timed out asking the Phodder service to quit"),
    }
}

async fn call_without_starting<B>(connection: &zbus::Connection, method: &str, no_reply: bool, body: &B) -> zbus::Result<()>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    let flags = if no_reply { MethodFlags::NoAutoStart | MethodFlags::NoReplyExpected } else { MethodFlags::NoAutoStart.into() };
    let proxy: zbus::Proxy<'_> = zbus::proxy::Builder::new(connection)
        .destination(BUS_NAME)?
        .path(OBJECT_PATH)?
        .interface(INTERFACE)?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await?;
    proxy.call_with_flags::<_, _, ()>(method, flags, body).await.map(|_| ())
}
