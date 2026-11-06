//! Fan-out of applied changes to live-tail subscribers. Publishing never blocks: a full ring
//! overwrites the oldest change, and lagging subscribers learn about it on their next receive.

use tokio::sync::broadcast;

use crate::store::Change;

pub const DEFAULT_CAPACITY: usize = 4096;

#[derive(Clone)]
pub struct Hub {
    tx: broadcast::Sender<Change>,
}

impl Hub {
    pub fn new(capacity: usize) -> Hub {
        let (tx, _) = broadcast::channel(capacity);
        Hub { tx }
    }

    pub fn publish(&self, change: Change) {
        let _ = self.tx.send(change);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Change> {
        self.tx.subscribe()
    }
}
