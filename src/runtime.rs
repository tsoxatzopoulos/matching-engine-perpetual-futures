//! Runs the engine on a dedicated thread behind two SPSC ring buffers.
//!
//! The engine itself is single-threaded and deterministic; this wrapper is the
//! sequencer. Commands go in through one ring, events come out through
//! another, one event per slot tagged with the command's sequence number, so
//! nothing is allocated per command. The engine thread busy-spins while idle:
//! lowest latency, at the cost of keeping one core at 100%.

use std::collections::VecDeque;
use std::thread::{self, JoinHandle};

use crate::engine::{Command, Engine};
use crate::events::Event;
use crate::spsc::{self, Consumer, Producer};

/// The engine thread has exited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EngineStopped;

/// One item of the engine's output stream.
#[derive(Clone, Debug, PartialEq)]
pub enum Output {
    /// An event produced by command number `seq` (1-based, in arrival order).
    Event { seq: u64, event: Event },
    /// Command `seq` is fully processed; all its events came before this.
    Done { seq: u64 },
}

pub struct EngineHandle {
    commands: Producer<Command>,
    events: Consumer<Output>,
    /// Outputs moved out of the event ring while `send` waited for room.
    /// Without this, a caller that sends without receiving would deadlock: it
    /// spins on a full command ring while the engine spins on a full event ring.
    backlog: VecDeque<Output>,
    thread: Option<JoinHandle<Engine>>,
}

impl EngineHandle {
    /// Starts the engine thread. `command_capacity` and `event_capacity` are
    /// rounded up to powers of two. The event ring should be much larger than
    /// the command ring: a single command can emit many events, and the engine
    /// waits while the event ring is full.
    pub fn spawn(engine: Engine, command_capacity: usize, event_capacity: usize) -> Self {
        let (commands, mut inbox) = spsc::channel::<Command>(command_capacity.max(1).next_power_of_two());
        let (mut outbox, events) = spsc::channel::<Output>(event_capacity.max(2).next_power_of_two());
        let thread = thread::Builder::new()
            .name("matching-engine".into())
            .spawn(move || {
                let mut engine = engine;
                let mut seq = 0u64;
                // `pop` returns None once the handle dropped its producer and
                // every queued command has been processed.
                while let Some(cmd) = inbox.pop() {
                    seq += 1;
                    for event in engine.apply(cmd) {
                        // Err means nobody listens any more; keep processing.
                        let _ = outbox.push(Output::Event { seq, event: event.clone() });
                    }
                    let _ = outbox.push(Output::Done { seq });
                }
                engine
            })
            .expect("spawn engine thread");
        Self { commands, events, backlog: VecDeque::new(), thread: Some(thread) }
    }

    /// Enqueues a command, busy-spinning while the command ring is full.
    /// While it waits it keeps the engine unblocked by moving pending outputs
    /// to an internal backlog, which `recv` serves first.
    pub fn send(&mut self, mut cmd: Command) -> Result<(), EngineStopped> {
        loop {
            match self.commands.try_push(cmd) {
                Ok(()) => return Ok(()),
                Err(back) => {
                    if self.commands.is_closed() {
                        return Err(EngineStopped);
                    }
                    cmd = back;
                    let mut moved = false;
                    while let Some(out) = self.events.try_pop() {
                        self.backlog.push_back(out);
                        moved = true;
                    }
                    if !moved {
                        std::hint::spin_loop();
                    }
                }
            }
        }
    }

    /// Enqueues a command if there is room; gives it back (boxed) otherwise.
    pub fn try_send(&mut self, cmd: Command) -> Result<(), Box<Command>> {
        self.commands.try_push(cmd).map_err(Box::new)
    }

    /// Next output item, busy-spinning until one is available. `None` once the
    /// engine thread has stopped and the stream is drained.
    pub fn recv(&mut self) -> Option<Output> {
        self.backlog.pop_front().or_else(|| self.events.pop())
    }

    /// Next output item if one is ready.
    pub fn try_recv(&mut self) -> Option<Output> {
        self.backlog.pop_front().or_else(|| self.events.try_pop())
    }

    /// Lets the engine finish every queued command, then stops the thread and
    /// returns the engine state. Outputs not yet received are discarded.
    pub fn shutdown(self) -> Engine {
        let Self { commands, mut events, mut thread, .. } = self;
        drop(commands);
        let thread = thread.take().expect("engine thread");
        // Keep draining so the engine never blocks on a full event ring.
        while !thread.is_finished() {
            if events.try_pop().is_none() {
                std::hint::spin_loop();
            }
        }
        thread.join().expect("engine thread panicked")
    }
}
