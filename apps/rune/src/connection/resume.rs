use serde_json::{Value, json};
use uuid::Uuid;

use super::{Command, Connection, Event, EventSender, Session, SessionStop};

pub(super) struct PendingResume {
    thread_id: String,
    operation: Uuid,
    release_queue: bool,
}

impl PendingResume {
    pub(super) fn finish(self, result: Result<(), String>) -> Event {
        Event::ResumeSettled {
            thread_id: self.thread_id,
            operation: self.operation,
            result,
        }
    }
}

impl Connection {
    pub(crate) fn resume_thread(&self, thread_id: String, source_run: String, operation: Uuid) {
        let _ = self.commands.send(Command::ResumeThread {
            thread_id,
            source_run,
            operation,
        });
    }
}

impl Session {
    pub(super) async fn submit_resume(
        &mut self,
        thread_id: String,
        source_run: String,
        operation: Uuid,
        events: &EventSender<impl Fn()>,
    ) -> Result<(), SessionStop> {
        let pending = self
            .pending_resumes
            .values()
            .any(|resume| resume.thread_id == thread_id);
        let available = self.thread.as_ref().filter(|thread| {
            self.worktree_queue
                && self.prompt_writes
                && !pending
                && thread.thread_id == thread_id
                && thread.reported_resumable_run.as_deref() == Some(source_run.as_str())
        });
        let Some(thread) = available else {
            events.send(Event::ResumeSettled {
                thread_id,
                operation,
                result: Err("This thread can no longer be resumed from that run.".into()),
            });
            return Ok(());
        };
        let release_queue = thread.reported_queue.iter().any(|queued| queued.held);
        let payload = json!({
            "type":"message.dispatch", "commandId":operation.to_string(),
            "threadId":thread_id, "messageId":Uuid::new_v4().to_string(),
            "createdBy":"user", "creationSource":"web", "attachments":[],
            "text":"Continue where you left off.", "manualContinuationOfRunId":source_run,
            "dispatchMode":{"type":"start_immediately"},
        });
        let request_id = self.allocate_id();
        self.pending_resumes.insert(
            request_id.clone(),
            PendingResume {
                thread_id,
                operation,
                release_queue,
            },
        );
        self.send_request(&request_id, "orchestration.dispatchCommand", payload)
            .await
    }

    pub(super) async fn finish_resume(
        &mut self,
        mut resume: PendingResume,
        receipt: Result<Value, String>,
        events: &EventSender<impl Fn()>,
    ) -> Result<(), SessionStop> {
        let result = receipt.and_then(|receipt| {
            receipt["sequence"]
                .as_u64()
                .map(|_| ())
                .ok_or_else(|| "The environment could not confirm resuming the thread.".to_owned())
        });
        if result.is_ok() && resume.release_queue {
            resume.release_queue = false;
            let payload = json!({
                "type":"queue.resume", "threadId":resume.thread_id,
                "commandId":format!("command:resume-queue:{}",resume.operation),
            });
            let request_id = self.allocate_id();
            self.pending_resumes.insert(request_id.clone(), resume);
            self.send_request(&request_id, "orchestration.dispatchCommand", payload)
                .await
        } else {
            events.send(resume.finish(result));
            Ok(())
        }
    }
}
