//! Ordered subscriptions: subscribe first, then replay past a known waterline.
use super::{RuntimeEvent, SessionRuntime};
use std::collections::VecDeque;

pub struct RuntimeSubscription {
    runtime: SessionRuntime,
    receiver: tokio::sync::broadcast::Receiver<RuntimeEvent>,
    pending: VecDeque<RuntimeEvent>,
    cursor: u64,
    closed: tokio::sync::watch::Receiver<bool>,
    replay: bool,
}
impl SessionRuntime {
    pub fn subscribe_from(&self, sequence: u64) -> RuntimeSubscription {
        RuntimeSubscription {
            runtime: self.clone(),
            closed: self.core.closed.subscribe(),
            receiver: self.subscribe(),
            pending: VecDeque::new(),
            cursor: sequence,
            replay: true,
        }
    }
    pub async fn replay_after(&self, sequence: u64) -> Result<Vec<RuntimeEvent>, String> {
        let waterline = self.snapshot().sequence;
        let events = if let Some(journal) = self.journal() {
            tokio::task::spawn_blocking(move || journal.events_after(sequence))
                .await
                .map_err(|error| error.to_string())?
                .map_err(|error| error.to_string())?
        } else {
            self.core
                .history
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .iter()
                .filter(|event| event.sequence.is_some_and(|value| value > sequence))
                .cloned()
                .collect()
        };
        if waterline > sequence
            && events.first().and_then(|event| event.sequence) != Some(sequence + 1)
        {
            return Err(
                "session event history is unavailable; reload the session projection".into(),
            );
        }
        Ok(events)
    }
}
impl RuntimeSubscription {
    pub fn sequence(&self) -> u64 {
        self.cursor
    }
    pub async fn next(&mut self) -> Result<RuntimeEvent, String> {
        loop {
            if *self.closed.borrow() {
                return Err("session runtime stopped".into());
            }
            if self.replay {
                self.pending = self.runtime.replay_after(self.cursor).await?.into();
                self.replay = false;
            }
            let event = if let Some(event) = self.pending.pop_front() {
                event
            } else {
                match tokio::select! {event=self.receiver.recv()=>event,_=self.closed.changed()=>return Err("session runtime stopped".into())}
                {
                    Ok(event) => event,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        self.replay = true;
                        continue;
                    }
                    Err(error) => return Err(error.to_string()),
                }
            };
            if let Some(sequence) = event.sequence {
                if sequence <= self.cursor {
                    continue;
                }
                if sequence != self.cursor + 1 {
                    self.replay = true;
                    continue;
                }
                self.cursor = sequence;
            }
            return Ok(event);
        }
    }
}
