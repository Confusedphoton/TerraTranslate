//! Native Linux text capture through the AT-SPI accessibility bus.
//!
//! Wayland's screen-cast portal deliberately provides pixels rather than another process's UI
//! tree. AT-SPI is the consented desktop interface for assistive technology and is consequently
//! the native counterpart to the Wine hook bridge.

use std::sync::{Arc, RwLock, mpsc};

use atspi::events::object::TextChangedEvent;
use atspi::proxy::accessible::ObjectRefExt;
use atspi_connection::AccessibilityConnection;
use futures_util::{StreamExt, pin_mut};

#[derive(Debug, thiserror::Error)]
#[error("AT-SPI accessibility error: {0}")]
pub struct NativeAccessibilityError(String);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeApplication {
    pub id: String,
    pub name: String,
}

fn is_atspi_service_unavailable(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    error.contains("org.freedesktop.dbus.error.serviceunknown") || error.contains("not activatable")
}

/// List applications currently exposed by the user's AT-SPI accessibility bus.
pub async fn list_native_applications() -> Result<Vec<NativeApplication>, NativeAccessibilityError>
{
    let connection = match AccessibilityConnection::new().await {
        Ok(connection) => connection,
        Err(error) => {
            let error = error.to_string();
            if is_atspi_service_unavailable(&error) {
                return Ok(Vec::new());
            }
            return Err(NativeAccessibilityError(error));
        }
    };
    // The registry's Accessible implementation is intentionally incomplete.
    // This helper disables zbus property caching, unlike a default proxy builder.
    let root = match connection.root_accessible_on_registry().await {
        Ok(root) => root,
        Err(error) if is_atspi_service_unavailable(&error.to_string()) => return Ok(Vec::new()),
        Err(error) => return Err(NativeAccessibilityError(error.to_string())),
    };
    let children = match root.get_children().await {
        Ok(children) => children,
        Err(error) if is_atspi_service_unavailable(&error.to_string()) => return Ok(Vec::new()),
        Err(error) => return Err(NativeAccessibilityError(error.to_string())),
    };
    let mut applications = Vec::new();
    for child in children {
        let Some(id) = child.name_as_str() else {
            continue;
        };

        // Registry children are the authoritative application snapshot. Some
        // toolkits expose only part of Accessible, and applications can vanish
        // while metadata is queried. Neither condition should hide the other
        // healthy applications (or poison the entire refresh).
        let name = match child.as_accessible_proxy(connection.connection()).await {
            Ok(proxy) => proxy.name().await.unwrap_or_else(|_| id.to_owned()),
            Err(_) => id.to_owned(),
        };
        applications.push(NativeApplication {
            id: id.to_owned(),
            name,
        });
    }
    normalize_native_applications(&mut applications);
    Ok(applications)
}

fn normalize_native_applications(applications: &mut Vec<NativeApplication>) {
    applications.sort_by(|left, right| left.id.cmp(&right.id));
    applications.dedup_by(|left, right| left.id == right.id);
    applications.sort_by(|left, right| {
        left.name
            .to_ascii_lowercase()
            .cmp(&right.name.to_ascii_lowercase())
            .then(left.name.cmp(&right.name))
            .then(left.id.cmp(&right.id))
    });
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

#[cfg(test)]
mod tests {
    use super::{NativeApplication, is_atspi_service_unavailable, normalize_native_applications};

    #[test]
    fn recognizes_missing_or_unactivatable_atspi_service() {
        assert!(is_atspi_service_unavailable(
            "org.freedesktop.DBus.Error.ServiceUnknown: The name is not activatable"
        ));
        assert!(is_atspi_service_unavailable(
            "org.freedesktop.DBus.Error.ServiceUnknown"
        ));
        assert!(is_atspi_service_unavailable("The name is not activatable"));
    }

    #[test]
    fn preserves_unrelated_atspi_errors() {
        assert!(!is_atspi_service_unavailable(
            "org.freedesktop.DBus.Error.AccessDenied: denied"
        ));
    }

    #[test]
    fn preserves_distinct_applications_with_duplicate_names() {
        let mut applications = vec![
            NativeApplication {
                id: ":1.9".into(),
                name: "Game".into(),
            },
            NativeApplication {
                id: ":1.4".into(),
                name: "game".into(),
            },
            NativeApplication {
                id: ":1.3".into(),
                name: "Game".into(),
            },
        ];

        normalize_native_applications(&mut applications);

        assert_eq!(
            applications
                .iter()
                .map(|application| application.id.as_str())
                .collect::<Vec<_>>(),
            vec![":1.3", ":1.9", ":1.4"]
        );
    }

    #[test]
    fn removes_duplicate_registry_references_by_application_id() {
        let mut applications = vec![
            NativeApplication {
                id: ":1.7".into(),
                name: ":1.7".into(),
            },
            NativeApplication {
                id: ":1.7".into(),
                name: "Example".into(),
            },
        ];

        normalize_native_applications(&mut applications);

        assert_eq!(applications.len(), 1);
    }
}
