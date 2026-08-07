use ashpd::desktop::Session;
use ashpd::desktop::global_shortcuts::{BindShortcutsOptions, GlobalShortcuts, NewShortcut};

#[derive(Clone, Debug)]
pub struct ShortcutBinding {
    pub id: String,
    pub description: String,
    pub preferred_trigger: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ShortcutError {
    #[error("desktop portal error: {0}")]
    Portal(#[from] ashpd::Error),
    #[error("the compositor rejected every requested shortcut")]
    NoneAccepted,
}

#[derive(Debug)]
pub struct PortalShortcutSession {
    _session: Session<GlobalShortcuts>,
    accepted_ids: Vec<String>,
}

impl PortalShortcutSession {
    pub fn accepted_ids(&self) -> &[String] {
        &self.accepted_ids
    }
}

pub async fn register_shortcuts(
    bindings: &[ShortcutBinding],
) -> Result<PortalShortcutSession, ShortcutError> {
    let portal = GlobalShortcuts::new().await?;
    let session = portal.create_session(Default::default()).await?;
    let shortcuts = bindings
        .iter()
        .map(|binding| {
            NewShortcut::new(&binding.id, &binding.description)
                .preferred_trigger(binding.preferred_trigger.as_str())
        })
        .collect::<Vec<_>>();
    let response = portal
        .bind_shortcuts(&session, &shortcuts, None, BindShortcutsOptions::default())
        .await?
        .response()?;
    let accepted_ids = response
        .shortcuts()
        .iter()
        .map(|shortcut| shortcut.id().to_owned())
        .collect::<Vec<_>>();
    if accepted_ids.is_empty() {
        return Err(ShortcutError::NoneAccepted);
    }
    Ok(PortalShortcutSession {
        _session: session,
        accepted_ids,
    })
}
