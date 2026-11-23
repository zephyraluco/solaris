use std::{path::Path, sync::Arc};

use anyhow::{Context, Result};
use solaris::application::{
    history::{find_session, open_store},
    session::{SessionTarget, open_journal},
};
use solaris_agent::{SessionJournal, SessionStore};

use super::{Args, Startup};

pub(super) enum HistoryCommand<'a> {
    List,
    Export(&'a str),
    Changes,
    Undo(&'a str),
}

impl HistoryCommand<'_> {
    pub(super) fn run(self, state_dir: Option<&Path>) -> Result<()> {
        let directory = state_dir.context("no configuration directory")?;
        match self {
            Self::Changes => {
                for change in solaris_tools::changes::list(&directory.join("sessions/artifacts"))? {
                    println!("{}  {}\n{}", change.id, change.path.display(), change.patch);
                }
            }
            Self::Undo(id) => {
                let change =
                    solaris_tools::changes::undo(&directory.join("sessions/artifacts"), id)?;
                println!("Restored {} ({})", change.path.display(), change.id);
            }
            Self::List => {
                let cwd = std::env::current_dir()?.canonicalize()?;
                for session in open_store(directory)?.list(&cwd)? {
                    println!("{}  {}  {}", session.id, session.status, session.model);
                }
            }
            Self::Export(id) => {
                let store = open_store(directory)?;
                let cwd = std::env::current_dir()?.canonicalize()?;
                find_session(&store, &cwd, id)?.context("session not found in this directory")?;
                println!("{}", serde_json::to_string_pretty(&store.records(id)?)?);
            }
        }
        Ok(())
    }
}

pub(super) fn prepare(startup: &mut Startup, args: &Args) -> Result<Option<Arc<SessionJournal>>> {
    if args.ephemeral || args.mcp_status {
        return Ok(None);
    }
    let directory = startup
        .state_dir
        .as_deref()
        .context("no configuration directory; use --ephemeral")?;
    let store = open_store(directory)?;
    let cwd = startup.resource_paths.cwd.canonicalize()?;
    let source = if args.continue_session {
        Some(
            store
                .list(&cwd)?
                .into_iter()
                .find(|info| !info.is_subagent)
                .context("no saved user session in this directory")?,
        )
    } else if let Some(id) = &args.resume {
        Some(find_session(&store, &cwd, id)?.context("session not found in this directory")?)
    } else if let Some(id) = &args.fork {
        Some(find_session(&store, &cwd, id)?.context("fork source not found in this directory")?)
    } else {
        None
    };

    let target_id = source.as_ref().map(|info| info.id.clone());
    if let Some(info) = source {
        args.connection_overrides()
            .restore(info, &mut startup.config.model, &mut startup.auth);
    }
    let (target, model) = match target_id.as_deref() {
        Some(id) if args.fork.is_some() => (SessionTarget::Fork(id), &startup.config.model),
        Some(id) => (SessionTarget::Resume(id), &startup.config.model),
        None => (SessionTarget::New, &startup.choice.model),
    };
    let journal = open_journal(&store, &cwd, model, target, None)?;
    Ok(Some(journal))
}

pub(super) fn acp_store(startup: &Startup) -> Result<Arc<SessionStore>> {
    let directory = startup
        .state_dir
        .as_deref()
        .context("no configuration directory")?;
    Ok(open_store(directory)?)
}
