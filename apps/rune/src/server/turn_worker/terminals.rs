use std::collections::HashSet;

use t3_rune::providers::codex::{
    CodexAdapterError, CodexSessionClient, ThreadId, ToolItem, ToolLifecycle,
};

#[derive(Default)]
pub(super) struct RunningTerminals {
    items: Vec<(String, String)>,
    contained: HashSet<String>,
}

impl RunningTerminals {
    pub(super) fn update(&mut self, item: &ToolItem, lifecycle: ToolLifecycle) {
        if let Some(index) = self.items.iter().position(|(id, _)| id == &item.id) {
            self.items.remove(index);
        }
        if lifecycle == ToolLifecycle::Started
            && let Some(process_id) = item.payload["processId"].as_str()
        {
            self.items.push((item.id.clone(), process_id.to_owned()));
        }
    }

    pub(super) fn pending(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        self.items
            .iter()
            .map(|(_, process_id)| process_id)
            .filter(|process_id| !self.contained.contains(*process_id) && seen.insert(*process_id))
            .cloned()
            .collect()
    }

    pub(super) fn record(&mut self, attempts: Vec<TerminalAttempt>) -> Option<CodexAdapterError> {
        let mut failure = None;
        for attempt in attempts {
            match attempt.result {
                Ok(()) => {
                    self.contained.insert(attempt.process_id);
                }
                Err(error) if failure.is_none() => failure = Some(error),
                Err(_) => {}
            }
        }
        failure
    }
}

pub(super) struct TerminalAttempt {
    process_id: String,
    result: Result<(), CodexAdapterError>,
}

pub(super) async fn terminate(
    client: &CodexSessionClient,
    thread_id: &ThreadId,
    process_ids: Vec<String>,
) -> Vec<TerminalAttempt> {
    let mut attempts = Vec::with_capacity(process_ids.len());
    for process_id in process_ids {
        let result = client
            .terminate_background_terminal(thread_id, &process_id)
            .await;
        attempts.push(TerminalAttempt { process_id, result });
    }
    attempts
}
