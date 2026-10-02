//! Runs the engine on a dedicated thread behind two SPSC ring buffers.
//!
//! The engine itself is single-threaded and deterministic; this wrapper is the
//! sequencer. Commands go in through one ring, events come out through
//! another, one event per slot tagged with the command's sequence number, so
//! nothing is allocated per command. How threads wait on an empty or full ring
//! is a [`WaitStrategy`]: busy-spinning gives the lowest latency but keeps a
//! core at 100% even when idle; backing off frees the core.

use std::collections::VecDeque;
use std::hint::spin_loop;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::engine::{Command, Engine};
use crate::events::Event;
use crate::spsc::{self, Consumer, Producer};

/// How a thread waits while a ring is empty (or full).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum WaitStrategy {
    /// Busy-spin with `spin_loop()`: lowest latency, one core always at 100%.
    /// What a production venue wants on a dedicated core.
    #[default]
    Spin,
    /// Spin briefly, then `yield_now`, then sleep `max_sleep`. Costs nothing
    /// while commands keep coming. When the engine has been idle, the next
    /// command may wait up to about `max_sleep` plus the OS timer slack.
    Backoff { max_sleep: Duration },
}

impl WaitStrategy {
    /// Backoff with a 50 µs sleep, suited to simulations on a laptop.
    pub fn backoff() -> Self {
        Self::Backoff { max_sleep: Duration::from_micros(50) }
    }
}

const SPIN_STEPS: u32 = 64;
const YIELD_STEPS: u32 = 16;

/// Escalating wait for one blocking operation; reset by creating a new one.
struct Waiter {
    strategy: WaitStrategy,
    step: u32,
}

impl Waiter {
    fn new(strategy: WaitStrategy) -> Self {
        Self { strategy, step: 0 }
    }

    #[inline]
    fn wait(&mut self) {
        match self.strategy {
            WaitStrategy::Spin => spin_loop(),
            WaitStrategy::Backoff { max_sleep } => {
                if self.step < SPIN_STEPS {
                    spin_loop();
                } else if self.step < SPIN_STEPS + YIELD_STEPS {
                    thread::yield_now();
                } else {
                    thread::sleep(max_sleep);
                }
                self.step = self.step.saturating_add(1);
            }
        }
    }
}

fn pop_wait<T>(ring: &mut Consumer<T>, strategy: WaitStrategy) -> Option<T> {
    let mut waiter = Waiter::new(strategy);
    loop {
        if let Some(v) = ring.try_pop() {
            return Some(v);
        }
        if ring.is_closed() {
            return ring.try_pop();
        }
        waiter.wait();
    }
}

/// Re-arm batch run while the engine waits for commands.
const IDLE_REARM_BATCH: usize = 64;

/// Waits for the next command, using idle time to re-arm risk bands (which
/// leaves less work for the next `MarkPrice`).
fn next_command(inbox: &mut Consumer<Command>, engine: &mut Engine, strategy: WaitStrategy) -> Option<Command> {
    let mut waiter = Waiter::new(strategy);
    loop {
        if let Some(cmd) = inbox.try_pop() {
            return Some(cmd);
        }
        if inbox.is_closed() {
            return inbox.try_pop();
        }
        if engine.rearm_pending(IDLE_REARM_BATCH) == 0 {
            waiter.wait();
        }
    }
}

fn push_wait<T>(ring: &mut Producer<T>, mut value: T, strategy: WaitStrategy) -> Result<(), T> {
    let mut waiter = Waiter::new(strategy);
    loop {
        match ring.try_push(value) {
            Ok(()) => return Ok(()),
            Err(v) if ring.is_closed() => return Err(v),
            Err(v) => {
                value = v;
                waiter.wait();
            }
        }
    }
}

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
    wait: WaitStrategy,
    thread: Option<JoinHandle<Engine>>,
}

impl EngineHandle {
    /// Starts the engine thread. `command_capacity` and `event_capacity` are
    /// rounded up to powers of two. The event ring should be much larger than
    /// the command ring: a single command can emit many events, and the engine
    /// waits while the event ring is full.
    pub fn spawn(engine: Engine, command_capacity: usize, event_capacity: usize) -> Self {
        Self::spawn_with(engine, command_capacity, event_capacity, WaitStrategy::Spin)
    }

    /// Like [`EngineHandle::spawn`], with an explicit wait strategy for the
    /// engine thread and for the blocking `send` / `recv` of this handle.
    pub fn spawn_with(
        engine: Engine,
        command_capacity: usize,
        event_capacity: usize,
        wait: WaitStrategy,
    ) -> Self {
        let (commands, mut inbox) = spsc::channel::<Command>(command_capacity.max(1).next_power_of_two());
        let (mut outbox, events) = spsc::channel::<Output>(event_capacity.max(2).next_power_of_two());
        let thread = thread::Builder::new()
            .name("matching-engine".into())
            .spawn(move || {
                let mut engine = engine;
                let mut seq = 0u64;
                // `pop` returns None once the handle dropped its producer and
                // every queued command has been processed.
                while let Some(cmd) = next_command(&mut inbox, &mut engine, wait) {
                    seq += 1;
                    for event in engine.apply(cmd) {
                        // Err means nobody listens any more; keep processing.
                        let _ = push_wait(&mut outbox, Output::Event { seq, event: event.clone() }, wait);
                    }
                    let _ = push_wait(&mut outbox, Output::Done { seq }, wait);
                }
                engine
            })
            .expect("spawn engine thread");
        Self { commands, events, backlog: VecDeque::new(), wait, thread: Some(thread) }
    }

    /// Enqueues a command, busy-spinning while the command ring is full.
    /// While it waits it keeps the engine unblocked by moving pending outputs
    /// to an internal backlog, which `recv` serves first.
    pub fn send(&mut self, mut cmd: Command) -> Result<(), EngineStopped> {
        let mut waiter = Waiter::new(self.wait);
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
                    if moved {
                        waiter = Waiter::new(self.wait);
                    } else {
                        waiter.wait();
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
        match self.backlog.pop_front() {
            Some(out) => Some(out),
            None => pop_wait(&mut self.events, self.wait),
        }
    }

    /// Next output item if one is ready.
    pub fn try_recv(&mut self) -> Option<Output> {
        self.backlog.pop_front().or_else(|| self.events.try_pop())
    }

    /// Lets the engine finish every queued command, then stops the thread and
    /// returns the engine state. Outputs not yet received are discarded.
    pub fn shutdown(self) -> Engine {
        let Self { commands, mut events, mut thread, wait, .. } = self;
        drop(commands);
        let thread = thread.take().expect("engine thread");
        // Keep draining so the engine never blocks on a full event ring.
        let mut waiter = Waiter::new(wait);
        while !thread.is_finished() {
            if events.try_pop().is_some() {
                waiter = Waiter::new(wait);
            } else {
                waiter.wait();
            }
        }
        thread.join().expect("engine thread panicked")
    }
}
