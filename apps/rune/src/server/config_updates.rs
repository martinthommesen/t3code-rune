use std::sync::{Arc, Mutex, PoisonError};

use serde_json::Value;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

#[derive(Default)]
pub(super) struct Subscribers(Mutex<Vec<UnboundedSender<Arc<Value>>>>);

impl Subscribers {
    pub(super) fn subscribe(&self) -> UnboundedReceiver<Arc<Value>> {
        let (send, receiver) = unbounded_channel();
        let mut subscriptions = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        subscriptions.retain(|sender| !sender.is_closed());
        subscriptions.push(send);
        receiver
    }

    pub(super) fn publish(&self, change: &Value) {
        let value = Arc::new(change.clone());
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|sender| sender.send(Arc::clone(&value)).is_ok());
    }
}
