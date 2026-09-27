//! Runs the engine on a dedicated thread behind bounded channels.
//!
//! The engine itself is single-threaded and deterministic; this wrapper is the
//! sequencer. A gateway would replace the std channel with an SPSC ring buffer
//! and journal each command before handing it over.

use std::sync::mpsc::{channel, sync_channel, Receiver, SyncSender};
use std::thread::{self, JoinHandle};

use crate::engine::{Command, Engine};
use crate::events::Event;

/// The engine thread has exited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EngineStopped;

pub struct EngineHandle {
    tx: Option<SyncSender<Command>>,
    events: Receiver<(u64, Vec<Event>)>,
    thread: Option<JoinHandle<Engine>>,
}

impl EngineHandle {
    /// Starts the engine thread. `capacity` bounds the inbound queue so
    /// producers get back-pressure instead of unbounded memory growth.
    pub fn spawn(engine: Engine, capacity: usize) -> Self {
        let (tx, rx) = sync_channel::<Command>(capacity);
        let (etx, erx) = channel();
        let thread = thread::Builder::new()
            .name("matching-engine".into())
            .spawn(move || {
                let mut engine = engine;
                let mut seq = 0u64;
                while let Ok(cmd) = rx.recv() {
                    seq += 1;
                    let events = engine.process(cmd);
                    // A dropped receiver just means nobody listens any more.
                    let _ = etx.send((seq, events));
                }
                engine
            })
            .expect("spawn engine thread");
        Self { tx: Some(tx), events: erx, thread: Some(thread) }
    }

    /// Enqueues a command; blocks while the queue is full.
    pub fn send(&self, cmd: Command) -> Result<(), EngineStopped> {
        self.tx.as_ref().expect("engine running").send(cmd).map_err(|_| EngineStopped)
    }

    /// Output stream: (command sequence number, events of that command).
    pub fn events(&self) -> &Receiver<(u64, Vec<Event>)> {
        &self.events
    }

    /// Drains the queue, stops the thread and returns the engine state.
    pub fn shutdown(mut self) -> Engine {
        drop(self.tx.take());
        self.thread.take().expect("engine thread").join().expect("engine thread panicked")
    }
}
