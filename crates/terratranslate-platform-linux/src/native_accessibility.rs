//! Native Linux text capture through the AT-SPI accessibility bus.
//!
//! Wayland's screen-cast portal deliberately provides pixels rather than another process's UI
//! tree. AT-SPI is the consented desktop interface for assistive technology and is consequently
//! the native counterpart to the Wine hook bridge.

use std::sync::{Arc, RwLock, mpsc};

use atspi::AccessibilityConnection;
use atspi::events::object::TextChangedEvent;
use futures_util::{StreamExt, pin_mut};

#[derive(Debug, thiserror::Error)]
#[error("AT-SPI accessibility error: {0}")]
pub struct NativeAccessibilityError(String);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeApplication {
    pub id: String,
    pub name: String,
}

/// List applications currently exposed by the user's AT-SPI accessibility bus.
pub async fn list_native_applications() -> Result<Vec<NativeApplication>, NativeAccessibilityError>
{
    let connection = AccessibilityConnection::new()
        .await
        .map_err(|error| NativeAccessibilityError(error.to_string()))?;
    let root = atspi::proxy::accessible::AccessibleProxy::new(connection.connection())
        .await
        .map_err(|error| NativeAccessibilityError(error.to_string()))?;
    let children = root
        .get_children()
        .await
        .map_err(|error| NativeAccessibilityError(error.to_string()))?;
    let mut applications = Vec::new();
    for child in children {
        let Some(id) = child.name_as_str() else {
            continue;
        };
        let proxy = atspi::proxy::accessible::AccessibleProxy::builder(connection.connection())
            .destination(id)
            .map_err(|error| NativeAccessibilityError(error.to_string()))?
            .path(child.path_as_str())
            .map_err(|error| NativeAccessibilityError(error.to_string()))?
            .build()
            .await
            .map_err(|error| NativeAccessibilityError(error.to_string()))?;
        let name = proxy.name().await.unwrap_or_else(|_| id.to_owned());
        applications.push(NativeApplication {
            id: id.to_owned(),
            name,
        });
    }
    applications.sort_by(|left, right| left.name.cmp(&right.name).then(left.id.cmp(&right.id)));
    Ok(applications)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeTextEvent {
    /// AT-SPI unique bus name for the application that emitted the change.
    pub application_id: String,
    pub object_path: String,
    pub timestamp_ms: i64,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NativeTextHookEvent {
    Ready,
    Text(NativeTextEvent),
    Error(String),
}

/// Watches AT-SPI text-change events for one explicitly selected native application.
///
/// No text is forwarded until [`Self::select_application`] is called. This avoids treating the
/// accessibility bus as a global capture source. `application_id` is the unique AT-SPI bus name
/// associated with the chosen application/window.
pub struct NativeTextHookService {
    events: mpsc::Receiver<NativeTextHookEvent>,
    selected_application: Arc<RwLock<Option<String>>>,
}

impl NativeTextHookService {
    pub fn start() -> Self {
        let (events_tx, events) = mpsc::channel();
        let selected_application = Arc::new(RwLock::new(None));
        let filter = Arc::clone(&selected_application);
        let _ = std::thread::Builder::new()
            .name("terratranslate-atspi-text".into())
            .spawn(move || run_atspi_listener(filter, events_tx));
        Self {
            events,
            selected_application,
        }
    }

    /// Begin forwarding text from an AT-SPI application. Pass `None` to stop forwarding text.
    pub fn select_application(&self, application_id: Option<String>) {
        if let Ok(mut selected) = self.selected_application.write() {
            *selected = application_id.filter(|id| !id.trim().is_empty());
        }
    }

    pub fn try_recv(&self) -> Result<NativeTextHookEvent, mpsc::TryRecvError> {
        self.events.try_recv()
    }
}

fn run_atspi_listener(
    selected_application: Arc<RwLock<Option<String>>>,
    events: mpsc::Sender<NativeTextHookEvent>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = events.send(NativeTextHookEvent::Error(format!(
                "start AT-SPI runtime: {error}"
            )));
            return;
        }
    };
    runtime.block_on(async move {
        let connection = match AccessibilityConnection::new().await {
            Ok(connection) => connection,
            Err(error) => {
                let _ = events.send(NativeTextHookEvent::Error(format!(
                    "connect to AT-SPI accessibility bus: {error}"
                )));
                return;
            }
        };
        if let Err(error) = connection.register_event::<TextChangedEvent>().await {
            let _ = events.send(NativeTextHookEvent::Error(format!(
                "subscribe to AT-SPI text changes: {error}"
            )));
            return;
        }
        let _ = events.send(NativeTextHookEvent::Ready);
        let stream = connection.event_stream();
        pin_mut!(stream);
        while let Some(event) = stream.next().await {
            let Ok(event) = event else {
                continue;
            };
            let Ok(event) = TextChangedEvent::try_from(event) else {
                continue;
            };
            let Some(application_id) = event.item.name_as_str() else {
                continue;
            };
            let selected = selected_application
                .read()
                .ok()
                .and_then(|selected| selected.clone());
            if selected.as_deref() != Some(application_id) || event.text.is_empty() {
                continue;
            }
            let _ = events.send(NativeTextHookEvent::Text(NativeTextEvent {
                application_id: application_id.to_owned(),
                object_path: event.item.path_as_str().to_owned(),
                timestamp_ms: now_ms(),
                text: event.text,
            }));
        }
    });
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
