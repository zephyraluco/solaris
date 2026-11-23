//! Resource preparation and deferred publication; no UI component dependencies.
use solaris_agent::{ResourcePaths, ResourceSnapshot, Skills};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
pub enum Reload {
    Unavailable,
    Queued,
    Started,
}
pub struct ResourceController {
    pub skills: Skills,
    pub resource_paths: Option<ResourcePaths>,
    pub resource_rx: Option<UnboundedReceiver<std::io::Result<ResourceSnapshot>>>,
    pub reload_pending: bool,
    publication: Option<tokio::sync::oneshot::Receiver<std::io::Result<ResourceSnapshot>>>,
}
impl ResourceController {
    pub fn publication_pending(&self) -> bool {
        self.publication.is_some()
    }
    pub fn publish(
        &mut self,
        session: std::sync::Arc<solaris_agent::SessionRuntime>,
        snapshot: ResourceSnapshot,
    ) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.publication = Some(rx);
        let request = session.dispatch(solaris_agent::RuntimeCommand::ReplaceResources {
            resources: snapshot.clone(),
        });
        tokio::spawn(async move {
            let result = match request {
                Ok(receipt) => receipt.accepted().await.map(|_| snapshot),
                Err(error) => Err(error),
            }
            .map_err(std::io::Error::other);
            let _ = tx.send(result);
        });
    }
    pub fn poll_publication(&mut self) -> Option<std::io::Result<ResourceSnapshot>> {
        let rx = self.publication.as_mut()?;
        match rx.try_recv() {
            Ok(result) => {
                self.publication = None;
                Some(result)
            }
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => None,
            Err(error) => {
                self.publication = None;
                Some(Err(std::io::Error::other(error)))
            }
        }
    }

    pub fn new(skills: Skills, resource_paths: Option<ResourcePaths>) -> Self {
        Self {
            skills,
            resource_paths,
            resource_rx: None,
            reload_pending: false,
            publication: None,
        }
    }
    pub fn reload(&mut self, busy: bool) -> Reload {
        let Some(paths) = self.resource_paths.clone() else {
            return Reload::Unavailable;
        };
        if busy || self.resource_rx.is_some() || self.publication.is_some() {
            self.reload_pending = true;
            return Reload::Queued;
        }
        self.reload_pending = false;
        let (tx, rx) = unbounded_channel();
        self.resource_rx = Some(rx);
        tokio::task::spawn_blocking(move || {
            let _ = tx.send(paths.load());
        });
        Reload::Started
    }
}

pub fn read_mcp_config(
    path: Option<&std::path::Path>,
    explicit: bool,
) -> anyhow::Result<solaris_mcp::McpConfig> {
    match path.map(std::fs::read_to_string) {
        Some(Ok(text)) => Ok(serde_json::from_str(&text)?),
        Some(Err(error)) if explicit || error.kind() != std::io::ErrorKind::NotFound => {
            Err(error.into())
        }
        _ => Ok(solaris_mcp::McpConfig::default()),
    }
}
