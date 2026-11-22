use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;

use super::{LocalOperations, RequestId, SharedEnvironment, chunk, failure, success};
use crate::server::project_clone::Snapshot as CloneSnapshot;
use crate::server::server_config;
use crate::server::terminal_rpc::{Subscription, observe};
use crate::server::thread_launch::subscribe_worktree_setup;
use crate::server::turn_worker::{CommandOutputSubscription, TurnQueue};
use crate::server::vcs_status;

struct Update {
    id: RequestId,
    generation: u64,
    frame: Value,
    done: bool,
}

pub(super) struct Streams {
    next_generation: u64,
    workers: HashMap<RequestId, (u64, JoinHandle<()>)>,
    send: mpsc::Sender<Update>,
    receive: mpsc::Receiver<Update>,
}

impl Streams {
    pub(super) fn new() -> Self {
        let (send, receive) = mpsc::channel(32);
        Self {
            next_generation: 0,
            workers: HashMap::new(),
            send,
            receive,
        }
    }

    pub(super) fn interrupt(&mut self, id: &RequestId) {
        if let Some((_, worker)) = self.workers.remove(id) {
            worker.abort();
        }
    }

    pub(super) fn start(
        &mut self,
        id: RequestId,
        mut subscription: Subscription,
        local: Arc<LocalOperations>,
    ) -> Value {
        self.interrupt(&id);
        let initial = chunk(id.clone(), std::mem::take(&mut subscription.initial));
        self.next_generation += 1;
        let generation = self.next_generation;
        let worker = tokio::spawn(forward(
            id.clone(),
            generation,
            subscription,
            local,
            self.send.clone(),
        ));
        self.workers.insert(id, (generation, worker));
        initial
    }

    pub(super) fn start_command_output(
        &mut self,
        id: RequestId,
        queue: TurnQueue,
        thread_id: String,
        item_id: String,
    ) {
        self.interrupt(&id);
        self.next_generation += 1;
        let generation = self.next_generation;
        let worker = tokio::spawn(forward_command_output(
            id.clone(),
            generation,
            queue,
            thread_id,
            item_id,
            self.send.clone(),
        ));
        self.workers.insert(id, (generation, worker));
    }

    pub(super) fn start_server_config(&mut self, id: RequestId, environment: SharedEnvironment) {
        self.interrupt(&id);
        self.next_generation += 1;
        let generation = self.next_generation;
        let request = id.clone();
        let send = self.send.clone();
        let local = environment
            .local
            .as_ref()
            .expect("local config subscription");
        let mut settings_changes = local.settings.subscribe_changes();
        let mut keybinding_changes = local.config.keybindings.subscribe();
        let worker = tokio::spawn(async move {
            let (frame, done) = match server_config::snapshot(&environment).await {
                Ok(snapshot) => (chunk(request.clone(), snapshot), false),
                Err(error) => (failure(request.clone(), error), true),
            };
            if send
                .send(Update {
                    id: request.clone(),
                    generation,
                    frame,
                    done,
                })
                .await
                .is_err()
                || done
            {
                return;
            }
            loop {
                let event = tokio::select! {
                    Some(settings) = settings_changes.recv() => json!({
                        "version": 1, "type": "settingsUpdated", "payload": {"settings": &*settings},
                    }),
                    Some(bindings) = keybinding_changes.recv() => json!({
                        "version": 1, "type": "keybindingsUpdated", "payload": &*bindings,
                    }),
                    else => break,
                };
                let frame = chunk(request.clone(), event);
                if send
                    .send(Update {
                        id: request.clone(),
                        generation,
                        frame,
                        done: false,
                    })
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
        self.workers.insert(id, (generation, worker));
    }

    pub(super) fn start_project_clones(
        &mut self,
        id: RequestId,
        mut snapshots: watch::Receiver<Vec<CloneSnapshot>>,
    ) -> Value {
        self.interrupt(&id);
        let initial = chunk(id.clone(), json!(&*snapshots.borrow_and_update()));
        self.next_generation += 1;
        let generation = self.next_generation;
        let request = id.clone();
        let send = self.send.clone();
        let worker = tokio::spawn(async move {
            while snapshots.changed().await.is_ok() {
                let frame = chunk(request.clone(), json!(&*snapshots.borrow_and_update()));
                if send
                    .send(Update {
                        id: request.clone(),
                        generation,
                        frame,
                        done: false,
                    })
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
        self.workers.insert(id, (generation, worker));
        initial
    }

    pub(super) fn start_worktree_setup(&mut self, id: RequestId, database: &Path, thread_id: &str) {
        self.interrupt(&id);
        let mut snapshots = subscribe_worktree_setup(database, thread_id);
        self.next_generation += 1;
        let generation = self.next_generation;
        let request = id.clone();
        let send = self.send.clone();
        let worker = tokio::spawn(async move {
            while let Some(snapshot) = snapshots.recv().await {
                if send
                    .send(Update {
                        id: request.clone(),
                        generation,
                        frame: chunk(request.clone(), snapshot),
                        done: false,
                    })
                    .await
                    .is_err()
                {
                    return;
                }
            }
            let _ = send
                .send(Update {
                    id: request.clone(),
                    generation,
                    frame: success(request, Value::Null),
                    done: true,
                })
                .await;
        });
        self.workers.insert(id, (generation, worker));
    }

    pub(super) fn start_vcs_status(
        &mut self,
        id: RequestId,
        service: vcs_status::Service,
        input: vcs_status::Input,
    ) {
        self.interrupt(&id);
        self.next_generation += 1;
        let generation = self.next_generation;
        let request = id.clone();
        let send = self.send.clone();
        let worker = tokio::spawn(async move {
            match service.subscribe(input).await {
                Ok(subscription) => {
                    forward_vcs_status(request, generation, subscription, send).await
                }
                Err(error) => {
                    let _ = send
                        .send(Update {
                            id: request.clone(),
                            generation,
                            frame: failure(request, error),
                            done: true,
                        })
                        .await;
                }
            }
        });
        self.workers.insert(id, (generation, worker));
    }

    pub(super) async fn next(&mut self) -> Value {
        loop {
            let update = self
                .receive
                .recv()
                .await
                .expect("terminal stream registry owns its channel");
            if self
                .workers
                .get(&update.id)
                .is_some_and(|(generation, _)| *generation == update.generation)
            {
                if update.done {
                    self.workers.remove(&update.id);
                }
                return update.frame;
            }
        }
    }
}

async fn forward_vcs_status(
    id: RequestId,
    generation: u64,
    mut subscription: vcs_status::Subscription,
    send: mpsc::Sender<Update>,
) {
    let mut next = Some(std::mem::take(&mut subscription.initial));
    loop {
        let Some(value) = next else { break };
        if send
            .send(Update {
                id: id.clone(),
                generation,
                frame: chunk(id.clone(), value),
                done: false,
            })
            .await
            .is_err()
        {
            return;
        }
        next = subscription.next().await;
    }
    let _ = send
        .send(Update {
            id: id.clone(),
            generation,
            frame: success(id, Value::Null),
            done: true,
        })
        .await;
}

async fn forward_command_output(
    id: RequestId,
    generation: u64,
    queue: TurnQueue,
    thread_id: String,
    item_id: String,
    send: mpsc::Sender<Update>,
) {
    let publish = |frame, done| {
        send.send(Update {
            id: id.clone(),
            generation,
            frame,
            done,
        })
    };
    let subscribed =
        tokio::task::spawn_blocking(move || queue.subscribe_command_output(&thread_id, &item_id))
            .await;
    let mut subscription: CommandOutputSubscription = match subscribed {
        Ok(Ok(subscription)) => subscription,
        Ok(Err(error)) => {
            let _ = publish(failure(id.clone(), error), true).await;
            return;
        }
        Err(_) => {
            let failure = super::defect(
                id.clone(),
                "Could not open the command output stream.".into(),
            );
            let _ = publish(failure, true).await;
            return;
        }
    };
    loop {
        match subscription.recv().await {
            Ok(Some(frame)) => {
                if publish(chunk(id.clone(), json!(frame)), false)
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Ok(None) => {
                let _ = publish(success(id.clone(), Value::Null), true).await;
                return;
            }
            Err(error) => {
                let _ = publish(failure(id.clone(), error), true).await;
                return;
            }
        }
    }
}

impl Drop for Streams {
    fn drop(&mut self) {
        for (_, worker) in self.workers.values() {
            worker.abort();
        }
    }
}

async fn forward(
    id: RequestId,
    generation: u64,
    mut subscription: Subscription,
    local: Arc<LocalOperations>,
    send: mpsc::Sender<Update>,
) {
    let publish = |frame, done| {
        send.send(Update {
            id: id.clone(),
            generation,
            frame,
            done,
        })
    };
    while let Some(events) = &mut subscription.events {
        match events.recv().await {
            Ok(event) => match serde_json::to_value(event) {
                Ok(event) => {
                    if publish(chunk(id.clone(), event), false).await.is_err() {
                        return;
                    }
                }
                Err(_) => {
                    let _ = publish(failure(id.clone(), json!({"_tag":"InvalidTerminalInput", "message":"Could not encode terminal output."})), true).await;
                    return;
                }
            },
            Err(broadcast::error::RecvError::Lagged(_)) => {
                match observe(
                    &local.terminals,
                    subscription.key.clone(),
                    subscription.max_history_bytes,
                ) {
                    Ok(next) => {
                        subscription = next;
                        if publish(
                            chunk(id.clone(), std::mem::take(&mut subscription.initial)),
                            false,
                        )
                        .await
                        .is_err()
                        {
                            return;
                        }
                    }
                    Err(error) => {
                        let _ = publish(failure(id.clone(), error), true).await;
                        return;
                    }
                }
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
    let _ = publish(success(id.clone(), Value::Null), true).await;
}
