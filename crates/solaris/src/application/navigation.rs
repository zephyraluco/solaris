//! Serialized session loading. Superseded results cannot replace the visible session.
use solaris_agent::{SessionInfo, SessionJournal, SessionRecord, SessionRuntime};
use solaris_provider::{AuthStore, BackendChoice};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::{Semaphore, oneshot};

use super::connection::replace_connection;
use super::history::{ConnectionOverrides, find_session};
use super::session::{SessionTarget, open_journal};

pub struct OpenSession {
    pub current: Arc<SessionJournal>,
    pub id: String,
    pub fork: bool,
    pub cwd: std::path::PathBuf,
    pub runtime: Arc<solaris_agent::Runtime>,
    pub config: super::config::Config,
    pub auth: AuthStore,
    pub factory: super::connection::BackendFactory,
    pub registry: Arc<solaris_tools::ToolRegistry>,
    pub tools: solaris_tools::ToolSelection,
    pub skills: solaris_agent::Skills,
    pub prompt: solaris_agent::PromptContext,
    pub approver: Arc<dyn solaris_agent::Approver>,
    pub execution: solaris_agent::ExecutionSettings,
}
impl OpenSession {
    fn load(self, overrides: ConnectionOverrides) -> Result<NavigationResult, String> {
        let Self {
            current,
            id,
            fork,
            cwd,
            runtime,
            mut config,
            mut auth,
            factory,
            registry,
            tools,
            skills,
            prompt,
            approver,
            execution,
        } = self;
        let cwd = cwd.canonicalize().unwrap_or(cwd);
        let info = find_session(current.store(), &cwd, &id)
            .map_err(|error| error.to_string())?
            .ok_or("session not found in this directory")?;
        overrides.restore(info, &mut config.model, &mut auth);
        let loaded = runtime.session(&id);
        let source = if current.id() == id {
            Some(Arc::clone(&current))
        } else {
            loaded.as_ref().and_then(|session| session.journal())
        };
        let target = if fork {
            SessionTarget::Fork(&id)
        } else {
            SessionTarget::Resume(&id)
        };
        let journal = open_journal(current.store(), &cwd, &config.model, target, source)
            .map_err(|error| error.to_string())?;
        let loaded = if fork { None } else { loaded };
        let choice = super::assembly::resolve_backend(&factory, &auth, &mut config);
        let session = if let Some(session) = loaded {
            let active = session.snapshot();
            if overrides.model
                || overrides.provider
                || active.model != choice.model
                || active.provider_id != choice.provider_id
            {
                let receipt =
                    session.dispatch(replace_connection(choice.clone(), &factory, &auth))?;
                // Navigation runs on a blocking worker. Wait for the runtime
                // to apply the connection before publishing the switch.
                tokio::runtime::Handle::current().block_on(receipt.accepted())?;
            }
            Arc::new(session)
        } else {
            let session = super::assembly::with_tools(
                choice.clone(),
                registry,
                &tools,
                &skills,
                &prompt,
                &cwd,
                approver,
                &execution,
                &factory,
                &auth,
                &runtime,
            )
            .map_err(|error| error.to_string())?;
            session
                .attach_journal(journal.clone())
                .map_err(|error| error.to_string())?;
            session
        };
        let (records, sequence) = journal.history().map_err(|error| error.to_string())?;
        Ok(NavigationResult::Open {
            session,
            records,
            sequence,
            config,
            auth,
            choice: Box::new(choice),
            fork,
        })
    }
}

pub enum NavigationResult {
    List {
        items: Vec<SessionInfo>,
        current: String,
        tree: bool,
    },
    Open {
        session: Arc<SessionRuntime>,
        records: Vec<SessionRecord>,
        sequence: u64,
        config: super::config::Config,
        auth: AuthStore,
        choice: Box<BackendChoice>,
        fork: bool,
    },
    History {
        records: Vec<SessionRecord>,
        sequence: u64,
    },
}
pub struct Navigation {
    generation: Arc<AtomicU64>,
    slots: Arc<Semaphore>,
    pending: Option<(u64, oneshot::Receiver<Result<NavigationResult, String>>)>,
}
impl Default for Navigation {
    fn default() -> Self {
        Self {
            generation: Arc::new(AtomicU64::new(0)),
            slots: Arc::new(Semaphore::new(1)),
            pending: None,
        }
    }
}
impl Navigation {
    pub fn open(&mut self, request: OpenSession) {
        self.open_with_overrides(request, ConnectionOverrides::default());
    }

    pub(crate) fn open_with_overrides(
        &mut self,
        request: OpenSession,
        overrides: ConnectionOverrides,
    ) {
        self.submit(move || request.load(overrides));
    }
    pub fn list(&mut self, journal: Arc<SessionJournal>, cwd: std::path::PathBuf, tree: bool) {
        self.submit(move || {
            let cwd = cwd.canonicalize().unwrap_or(cwd);
            let items = journal
                .store()
                .list(&cwd)
                .map_err(|error| error.to_string())?;
            Ok(NavigationResult::List {
                items,
                current: journal.id().to_owned(),
                tree,
            })
        });
    }

    pub fn cancel(&mut self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.pending = None;
    }
    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }
    pub fn submit(
        &mut self,
        work: impl FnOnce() -> Result<NavigationResult, String> + Send + 'static,
    ) {
        let id = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        let generation = self.generation.clone();
        let slots = self.slots.clone();
        let (tx, rx) = oneshot::channel();
        self.pending = Some((id, rx));
        tokio::spawn(async move {
            let Ok(_permit) = slots.acquire_owned().await else {
                return;
            };
            if generation.load(Ordering::Acquire) != id {
                return;
            }
            let result = tokio::task::spawn_blocking(work)
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result);
            if generation.load(Ordering::Acquire) == id {
                let _ = tx.send(result);
            }
        });
    }
    pub fn poll(&mut self) -> Option<Result<NavigationResult, String>> {
        let (id, rx) = self.pending.as_mut()?;
        match rx.try_recv() {
            Ok(result) => {
                let current = *id == self.generation.load(Ordering::Acquire);
                self.pending = None;
                current.then_some(result)
            }
            Err(oneshot::error::TryRecvError::Empty) => None,
            Err(error) => {
                self.pending = None;
                Some(Err(error.to_string()))
            }
        }
    }
    pub fn history(&mut self, journal: Arc<SessionJournal>) {
        self.submit(move || {
            journal
                .history()
                .map(|(records, sequence)| NavigationResult::History { records, sequence })
                .map_err(|error| error.to_string())
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn superseded_load_is_never_published_and_does_not_block_the_caller() {
        let mut navigation = Navigation::default();
        let (started, wait_started) = oneshot::channel();
        let (release, wait_release) = std::sync::mpsc::channel();
        navigation.submit(move || {
            let _ = started.send(());
            wait_release.recv().unwrap();
            Ok(NavigationResult::List {
                items: Vec::new(),
                current: "old".into(),
                tree: false,
            })
        });
        wait_started.await.unwrap();
        navigation.submit(|| {
            Ok(NavigationResult::List {
                items: Vec::new(),
                current: "new".into(),
                tree: false,
            })
        });
        assert!(navigation.poll().is_none());
        release.send(()).unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if let Some(result) = navigation.poll() {
                    break result.unwrap();
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(matches!(result,NavigationResult::List {current,..} if current=="new"));
    }
}
