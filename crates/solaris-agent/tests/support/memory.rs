use rig_core::message::Message;
use solaris_agent::SessionJournal;

fn store(journal: &SessionJournal) -> rig_memory::DirStore {
    let root = journal.store().artifact_dir();
    rig_memory::DirStore::open(root.parent().unwrap().join("memory").join(journal.id())).unwrap()
}

pub fn messages(journal: &SessionJournal) -> Vec<Message> {
    rig_memory::load(&store(journal), "conversation")
        .unwrap()
        .messages()
        .iter()
        .map(|message| message.as_ref().clone())
        .collect()
}

#[allow(dead_code)] // The subagent suite only reads its saved history.
pub fn seed(journal: &SessionJournal, messages: Vec<Message>) {
    let store = store(journal);
    let mut conversation = rig_memory::load(&store, "conversation").unwrap();
    conversation.extend(messages);
    rig_memory::save(&store, "conversation", &mut conversation).unwrap();
}
