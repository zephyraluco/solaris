//! Directory-scoped access to saved sessions and their connection identity.

use std::{path::Path, sync::Arc};

use solaris_agent::{SessionInfo, SessionStore, StoreError};
use solaris_provider::AuthStore;

/// Find a session in a canonical working directory without acquiring its lease.
pub fn find_session(
    store: &SessionStore,
    cwd: &Path,
    id: &str,
) -> Result<Option<SessionInfo>, StoreError> {
    Ok(store
        .list(cwd)?
        .into_iter()
        .find(|session| session.id == id))
}

pub fn open_store(config_dir: &Path) -> Result<Arc<SessionStore>, StoreError> {
    SessionStore::open(&config_dir.join("sessions/sessions.sqlite"))
}

#[derive(Clone, Copy, Default)]
pub struct ConnectionOverrides {
    pub model: bool,
    pub provider: bool,
}

impl ConnectionOverrides {
    pub fn restore(self, info: SessionInfo, model: &mut String, auth: &mut AuthStore) {
        if !self.model {
            *model = info.model;
        }
        if !self.provider
            && let Some(provider) = info.provider
        {
            auth.activate(provider);
        }
    }
}
