//! Lock-free single-producer single-consumer ring buffer.
//!
//! One thread pushes, one thread pops. Each side owns one index and only reads
//! the other side's index, so no compare-and-swap is ever needed: a push or a
//! pop is a slot write/read plus one atomic store.
//!
//! Indices are free-running counters (they only grow, wrapping at `usize::MAX`).
//! `tail - head` is the number of items in the ring, and the slot of index `i`
//! is `i & mask`, which is why the capacity must be a power of two.

use std::cell::UnsafeCell;
use std::hint::spin_loop;
use std::mem::MaybeUninit;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Keeps a value on its own cache line pair. 128 bytes rather than 64: Apple
/// Silicon uses 128-byte lines, and Intel prefetches lines in adjacent pairs.
/// Without it the producer's `tail` and the consumer's `head` would share a
/// line and every store by one core would invalidate the other's cache (false
/// sharing).
#[repr(align(128))]
struct CachePadded<T>(T);

struct Ring<T> {
    /// Next index to read. Written only by the consumer.
    head: CachePadded<AtomicUsize>,
    /// Next index to write. Written only by the producer.
    tail: CachePadded<AtomicUsize>,
    producer_alive: AtomicBool,
    consumer_alive: AtomicBool,
    mask: usize,
    slots: Box<[UnsafeCell<MaybeUninit<T>>]>,
}

// SAFETY: a slot is only ever accessed by one thread at a time. The producer
// writes slots in `[tail, head + capacity)`, the consumer reads slots in
// `[head, tail)`, and those ranges never overlap. The Release/Acquire pairs on
// `head` and `tail` hand a slot over from one side to the other. Values of `T`
// move between threads, hence `T: Send`.
unsafe impl<T: Send> Sync for Ring<T> {}
unsafe impl<T: Send> Send for Ring<T> {}

impl<T> Ring<T> {
    #[inline]
    fn slot(&self, index: usize) -> *mut MaybeUninit<T> {
        self.slots[index & self.mask].get()
    }

    #[inline]
    fn capacity(&self) -> usize {
        self.mask + 1
    }
}

impl<T> Drop for Ring<T> {
    fn drop(&mut self) {
        // Both sides are gone (the last `Arc` is being dropped), so plain reads
        // are enough. Drop the items that were pushed but never popped.
        let head = *self.head.0.get_mut();
        let tail = *self.tail.0.get_mut();
        let mut i = head;
        while i != tail {
            // SAFETY: slots in [head, tail) were written by `push` and not read.
            unsafe { (*self.slot(i)).assume_init_drop() };
            i = i.wrapping_add(1);
        }
    }
}

/// Writing end. Not `Clone`: there is exactly one producer.
pub struct Producer<T> {
    ring: Arc<Ring<T>>,
    /// Local copy of `tail` (this side is its only writer).
    tail: usize,
    /// Last `head` seen. Re-read only when the ring looks full.
    cached_head: usize,
}

/// Reading end. Not `Clone`: there is exactly one consumer.
pub struct Consumer<T> {
    ring: Arc<Ring<T>>,
    /// Local copy of `head` (this side is its only writer).
    head: usize,
    /// Last `tail` seen. Re-read only when the ring looks empty.
    cached_tail: usize,
}

/// Creates a ring with room for `capacity` items. Panics unless `capacity` is
/// a power of two.
pub fn channel<T>(capacity: usize) -> (Producer<T>, Consumer<T>) {
    assert!(capacity.is_power_of_two(), "SPSC capacity must be a power of two, got {capacity}");
    let slots = (0..capacity).map(|_| UnsafeCell::new(MaybeUninit::uninit())).collect();
    let ring = Arc::new(Ring {
        head: CachePadded(AtomicUsize::new(0)),
        tail: CachePadded(AtomicUsize::new(0)),
        producer_alive: AtomicBool::new(true),
        consumer_alive: AtomicBool::new(true),
        mask: capacity - 1,
        slots,
    });
    (
        Producer { ring: Arc::clone(&ring), tail: 0, cached_head: 0 },
        Consumer { ring, head: 0, cached_tail: 0 },
    )
}

impl<T> Producer<T> {
    pub fn capacity(&self) -> usize {
        self.ring.capacity()
    }

    /// Pushes without waiting; gives the value back if the ring is full.
    #[inline]
    pub fn try_push(&mut self, value: T) -> Result<(), T> {
        if self.tail.wrapping_sub(self.cached_head) == self.ring.capacity() {
            // Acquire pairs with the consumer's Release store of `head`: every
            // read the consumer did from the slots it released happens-before
            // our overwrite below. Without it, the write could race with the
            // consumer still copying the old value out of the same slot.
            self.cached_head = self.ring.head.0.load(Ordering::Acquire);
            if self.tail.wrapping_sub(self.cached_head) == self.ring.capacity() {
                return Err(value);
            }
        }
        // SAFETY: the slot at `tail` is outside [head, tail), so the consumer
        // does not touch it, and it is empty (either never written or already
        // read, as established by the Acquire load above).
        unsafe { (*self.ring.slot(self.tail)).write(value) };
        self.tail = self.tail.wrapping_add(1);
        // Release publishes the slot write: a consumer that Acquire-loads this
        // new `tail` is guaranteed to see the fully written value.
        self.ring.tail.0.store(self.tail, Ordering::Release);
        Ok(())
    }

    /// Pushes, busy-spinning while the ring is full. Fails only if the
    /// consumer has been dropped, giving the value back.
    pub fn push(&mut self, mut value: T) -> Result<(), T> {
        loop {
            match self.try_push(value) {
                Ok(()) => return Ok(()),
                Err(v) => {
                    // Acquire pairs with the Release in `Consumer::drop`.
                    if !self.ring.consumer_alive.load(Ordering::Acquire) {
                        return Err(v);
                    }
                    value = v;
                    spin_loop();
                }
            }
        }
    }

    /// True once the consumer has been dropped.
    pub fn is_closed(&self) -> bool {
        !self.ring.consumer_alive.load(Ordering::Acquire)
    }
}

impl<T> Drop for Producer<T> {
    fn drop(&mut self) {
        // Release orders this after our last `tail` store, so a consumer that
        // sees `producer_alive == false` (Acquire) and then reloads `tail`
        // finds every item we pushed.
        self.ring.producer_alive.store(false, Ordering::Release);
    }
}

impl<T> Consumer<T> {
    pub fn capacity(&self) -> usize {
        self.ring.capacity()
    }

    /// Pops without waiting.
    #[inline]
    pub fn try_pop(&mut self) -> Option<T> {
        if self.head == self.cached_tail {
            // Acquire pairs with the producer's Release store of `tail`: the
            // slot writes up to that index are visible to us.
            self.cached_tail = self.ring.tail.0.load(Ordering::Acquire);
            if self.head == self.cached_tail {
                return None;
            }
        }
        // SAFETY: `head` is inside [head, tail), so the slot holds a value
        // fully written by the producer (Acquire above) that nobody else reads.
        let value = unsafe { (*self.ring.slot(self.head)).assume_init_read() };
        self.head = self.head.wrapping_add(1);
        // Release orders our read of the slot before the producer can see the
        // slot as free (its Acquire load of `head`) and overwrite it.
        self.ring.head.0.store(self.head, Ordering::Release);
        Some(value)
    }

    /// Pops, busy-spinning while the ring is empty. Returns `None` once the
    /// producer has been dropped and every item has been taken.
    pub fn pop(&mut self) -> Option<T> {
        loop {
            if let Some(v) = self.try_pop() {
                return Some(v);
            }
            // Acquire pairs with the Release in `Producer::drop`; the retry
            // then sees every item pushed before the drop.
            if !self.ring.producer_alive.load(Ordering::Acquire) {
                return self.try_pop();
            }
            spin_loop();
        }
    }

    /// True once the producer has been dropped (items may still be queued).
    pub fn is_closed(&self) -> bool {
        !self.ring.producer_alive.load(Ordering::Acquire)
    }
}

impl<T> Drop for Consumer<T> {
    fn drop(&mut self) {
        self.ring.consumer_alive.store(false, Ordering::Release);
    }
}

// SAFETY: each end is used by one thread at a time (`&mut self` methods) and
// only moves `T` values across threads.
unsafe impl<T: Send> Send for Producer<T> {}
unsafe impl<T: Send> Send for Consumer<T> {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn empty_and_full() {
        let (mut p, mut c) = channel::<u32>(4);
        assert_eq!(c.try_pop(), None);
        for i in 0..4 {
            p.try_push(i).unwrap();
        }
        assert_eq!(p.try_push(99), Err(99), "fifth push must fail");
        assert_eq!(c.try_pop(), Some(0));
        p.try_push(4).unwrap();
        assert_eq!((1..=4).map(|_| c.try_pop().unwrap()).collect::<Vec<_>>(), vec![1, 2, 3, 4]);
        assert_eq!(c.try_pop(), None);
    }

    #[test]
    fn wraps_around_many_times() {
        let (mut p, mut c) = channel::<usize>(8);
        for round in 0..1_000 {
            for k in 0..5 {
                p.try_push(round * 5 + k).unwrap();
            }
            for k in 0..5 {
                assert_eq!(c.try_pop(), Some(round * 5 + k));
            }
        }
    }

    #[test]
    fn index_wraps_at_usize_max() {
        let (mut p, mut c) = channel::<u8>(4);
        let start = usize::MAX - 2;
        p.tail = start;
        p.cached_head = start;
        c.head = start;
        c.cached_tail = start;
        p.ring.head.0.store(start, Ordering::Relaxed);
        p.ring.tail.0.store(start, Ordering::Relaxed);
        for i in 0..10u8 {
            p.try_push(i).unwrap();
            assert_eq!(c.try_pop(), Some(i));
        }
    }

    #[test]
    #[should_panic(expected = "power of two")]
    fn capacity_must_be_power_of_two() {
        let _ = channel::<u8>(6);
    }

    struct Counted(Arc<AtomicUsize>);

    impl Drop for Counted {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn drops_items_left_in_the_ring() {
        let drops = Arc::new(AtomicUsize::new(0));
        let (mut p, mut c) = channel(8);
        for _ in 0..5 {
            p.try_push(Counted(Arc::clone(&drops))).ok().unwrap();
        }
        drop(c.try_pop()); // one popped and dropped by us
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        drop(p);
        drop(c);
        assert_eq!(drops.load(Ordering::Relaxed), 5, "the 4 queued items must be dropped with the ring");
    }

    #[test]
    fn disconnect() {
        let (mut p, mut c) = channel::<u32>(4);
        p.push(1).unwrap();
        drop(p);
        assert!(c.is_closed());
        assert_eq!(c.pop(), Some(1), "items pushed before the drop are still delivered");
        assert_eq!(c.pop(), None);

        let (mut p, c) = channel::<u32>(1);
        p.push(1).unwrap();
        drop(c);
        assert_eq!(p.push(2), Err(2), "push into a full ring without consumer fails");
    }

    #[test]
    fn two_threads_keep_order() {
        const N: u64 = 5_000_000;
        let (mut p, mut c) = channel::<u64>(1024);
        let producer = std::thread::spawn(move || {
            for i in 0..N {
                p.push(i).unwrap();
            }
        });
        let mut expected = 0;
        while let Some(v) = c.pop() {
            assert_eq!(v, expected, "out of order or lost item");
            expected += 1;
        }
        producer.join().unwrap();
        assert_eq!(expected, N);
    }

    #[test]
    fn two_threads_heap_values() {
        // Boxed values catch torn or double reads (a use-after-free would
        // crash or show up under Miri / sanitizers).
        const N: usize = 200_000;
        let (mut p, mut c) = channel::<Box<[usize; 4]>>(64);
        let producer = std::thread::spawn(move || {
            for i in 0..N {
                p.push(Box::new([i, i + 1, i + 2, i + 3])).unwrap();
            }
        });
        for i in 0..N {
            let v = c.pop().unwrap();
            assert_eq!(*v, [i, i + 1, i + 2, i + 3]);
        }
        assert_eq!(c.pop(), None);
        producer.join().unwrap();
    }
}
