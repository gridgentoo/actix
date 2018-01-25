//! This is copy of [sync/mpsc/](https://github.com/alexcrichton/futures-rs)
//!
//! A multi-producer, single-consumer, futures-aware, FIFO queue with back pressure.
//!
//! A channel can be used as a communication primitive between tasks running on
//! `futures-rs` executors. Channel creation provides `Receiver` and `Sender`
//! handles. `Receiver` implements `Stream` and allows a task to read values
//! out of the channel. If there is no message to read from the channel, the
//! current task will be notified when a new value is sent. `Sender` implements
//! the `Sink` trait and allows a task to send messages into the channel. If
//! the channel is at capacity, then send will be rejected and the task will be
//! notified when additional capacity is available.
//!
//! # Disconnection
//!
//! When all `Sender` handles have been dropped, it is no longer possible to
//! send values into the channel. This is considered the termination event of
//! the stream. As such, `Sender::poll` will return `Ok(Ready(None))`.
//!
//! If the receiver handle is dropped, then messages can no longer be read out
//! of the channel. In this case, a `send` will result in an error.
//!
//! # Clean Shutdown
//!
//! If the `Receiver` is simply dropped, then it is possible for there to be
//! messages still in the channel that will not be processed. As such, it is
//! usually desirable to perform a "clean" shutdown. To do this, the receiver
//! will first call `close`, which will prevent any further messages to be sent
//! into the channel. Then, the receiver consumes the channel to completion, at
//! which point the receiver can be dropped.

// At the core, the channel uses an atomic FIFO queue for message passing. This
// queue is used as the primary coordination primitive. In order to enforce
// capacity limits and handle back pressure, a secondary FIFO queue is used to
// send parked task handles.
//
// The general idea is that the channel is created with a `buffer` size of `n`.
// The channel capacity is `n + num-senders`. Each sender gets one "guaranteed"
// slot to hold a message. This allows `Sender` to know for a fact that a send
// will succeed *before* starting to do the actual work of sending the value.
// Since most of this work is lock-free, once the work starts, it is impossible
// to safely revert.
//
// If the sender is unable to process a send operation, then the current
// task is parked and the handle is sent on the parked task queue.
//
// Note that the implementation guarantees that the channel capacity will never
// exceed the configured limit, however there is no *strict* guarantee that the
// receiver will wake up a parked task *immediately* when a slot becomes
// available. However, it will almost always unpark a task when a slot becomes
// available and it is *guaranteed* that a sender will be unparked when the
// message that caused the sender to become parked is read out of the channel.
//
// The steps for sending a message are roughly:
//
// 1) Increment the channel message count
// 2) If the channel is at capacity, push the task handle onto the wait queue
// 3) Push the message onto the message queue.
//
// The steps for receiving a message are roughly:
//
// 1) Pop a message from the message queue
// 2) Pop a task handle from the wait queue
// 3) Decrement the channel message count.
//
// It's important for the order of operations on lock-free structures to happen
// in reverse order between the sender and receiver. This makes the message
// queue the primary coordination structure and establishes the necessary
// happens-before semantics required for the acquire / release semantics used
// by the queue structure.
#![allow(dead_code)]

use std::usize;
use std::thread;
use std::cell::Cell;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::{Relaxed, SeqCst};
use std::sync::{Arc, Mutex};

use futures::task::{self, Task};
use futures::{Async, Poll, Stream};
use futures::sync::oneshot::{channel as sync_channel, Receiver};

use super::queue::{Queue, PopResult};

use actor::Actor;
use address::SendError;
use handler::{Handler, ResponseType, MessageResult};
use envelope::{Envelope, ToEnvelope};

/// The transmission end of a channel which is used to send values.
///
/// This is created by the `channel` method.
pub struct AddressSender<A: Actor> {
    // Channel state shared between the sender and receiver.
    inner: Arc<Inner<A>>,

    // Handle to the task that is blocked on this sender. This handle is sent
    // to the receiver half in order to be notified when the sender becomes
    // unblocked.
    sender_task: Arc<Mutex<SenderTask>>,

    // True if the sender might be blocked. This is an optimization to avoid
    // having to lock the mutex most of the time.
    maybe_parked: Cell<bool>,
}

trait AssertKinds: Send + Sync + Clone {}


/// The receiving end of a channel which implements the `Stream` trait.
///
/// This is a concrete implementation of a stream which can be used to represent
/// a stream of values being computed elsewhere. This is created by the
/// `channel` method.
pub struct AddressReceiver<A: Actor> {
    inner: Arc<Inner<A>>,
}

struct Inner<A: Actor> {
    // Max buffer size of the channel. If `0` then the channel is unbounded.
    buffer: usize,

    // Internal channel state. Consists of the number of messages stored in the
    // channel as well as a flag signalling that the channel is closed.
    state: AtomicUsize,

    // Atomic, FIFO queue used to send messages to the receiver
    message_queue: Queue<Option<Envelope<A>>>,

    // Atomic, FIFO queue used to send parked task handles to the receiver.
    parked_queue: Queue<Arc<Mutex<SenderTask>>>,

    // Number of senders in existence
    num_senders: AtomicUsize,

    // Handle to the receiver's task.
    recv_task: Mutex<ReceiverTask>,
}

// Struct representation of `Inner::state`.
#[derive(Debug, Clone, Copy)]
struct State {
    // `true` when the channel is open
    is_open: bool,

    // Number of messages in the channel
    num_messages: usize,
}

#[derive(Debug)]
struct ReceiverTask {
    unparked: bool,
    task: Option<Task>,
}

// Returned from Receiver::try_park()
enum TryPark {
    Parked,
    Closed,
    NotEmpty,
}

// The `is_open` flag is stored in the left-most bit of `Inner::state`
const OPEN_MASK: usize = usize::MAX - (usize::MAX >> 1);

// When a new channel is created, it is created in the open state with no
// pending messages.
const INIT_STATE: usize = OPEN_MASK;

// The maximum number of messages that a channel can track is `usize::MAX >> 1`
const MAX_CAPACITY: usize = !(OPEN_MASK);

// The maximum requested buffer size must be less than the maximum capacity of
// a channel. This is because each sender gets a guaranteed slot.
const MAX_BUFFER: usize = MAX_CAPACITY >> 1;

// Sent to the consumer to wake up blocked producers
#[derive(Debug)]
struct SenderTask {
    task: Option<Task>,
    is_parked: bool,
}

impl SenderTask {
    fn new() -> Self {
        SenderTask {
            task: None,
            is_parked: false,
        }
    }

    fn notify(&mut self) {
        self.is_parked = false;

        if let Some(task) = self.task.take() {
            task.notify();
        }
    }
}

/// Creates an in-memory channel implementation of the `Stream` trait with
/// bounded capacity.
///
/// This method creates a concrete implementation of the `Stream` trait which
/// can be used to send values across threads in a streaming fashion. This
/// channel is unique in that it implements back pressure to ensure that the
/// sender never outpaces the receiver. The channel capacity is equal to
/// `buffer + num-senders`. In other words, each sender gets a guaranteed slot
/// in the channel capacity, and on top of that there are `buffer` "first come,
/// first serve" slots available to all senders.
///
/// The `Receiver` returned implements the `Stream` trait and has access to any
/// number of the associated combinators for transforming the result.
pub fn channel<A: Actor>(buffer: usize) -> (AddressSender<A>, AddressReceiver<A>) {
    // Check that the requested buffer size does not exceed the maximum buffer
    // size permitted by the system.
    assert!(buffer < MAX_BUFFER, "requested buffer size too large");

    let inner = Arc::new(Inner {
        buffer: buffer,
        state: AtomicUsize::new(INIT_STATE),
        message_queue: Queue::new(),
        parked_queue: Queue::new(),
        num_senders: AtomicUsize::new(1),
        recv_task: Mutex::new(ReceiverTask {
            unparked: false,
            task: None,
        }),
    });

    let tx = AddressSender {
        inner: Arc::clone(&inner),
        sender_task: Arc::new(Mutex::new(SenderTask::new())),
        maybe_parked: Cell::new(false),
    };

    let rx = AddressReceiver {
        inner: inner,
    };

    (tx, rx)
}

//
//
// ===== impl Sender =====
//
//
impl<A: Actor> AddressSender<A> {

    pub fn connected(&self) -> bool {
        let curr = self.inner.state.load(SeqCst);
        let state = decode_state(curr);

        state.is_open
    }

    /// Attempts to send a message on this `Sender<A>` with blocking.
    ///
    /// This function, must be called from inside of a task.
    pub fn send<M>(&self, msg: M) -> Result<Receiver<MessageResult<M>>, SendError<M>>
        where A: Handler<M>, <A as Actor>::Context: ToEnvelope<A>,
              M::Item: Send, M::Error: Send,
              M: ResponseType + Send + 'static,
    {
        // If the sender is currently blocked, reject the message
        if !self.poll_unparked(false).is_ready() {
            return Err(SendError::NotReady(msg))
        }

        // First, increment the number of messages contained by the channel.
        // This operation will also atomically determine if the sender task
        // should be parked.
        //
        // None is returned in the case that the channel has been closed by the
        // receiver. This happens when `Receiver::close` is called or the
        // receiver is dropped.
        let park_self = match self.inc_num_messages() {
            Some(park_self) => park_self,
            None => return Err(SendError::Closed(msg)),
        };

        // If the channel has reached capacity, then the sender task needs to
        // be parked. This will send the task handle on the parked task queue.
        if park_self {
            self.park(true);
            Err(SendError::NotReady(msg))
        } else {
            let (tx, rx) = sync_channel();
            let env = <A::Context as ToEnvelope<A>>::pack(msg, Some(tx));
            self.queue_push_and_signal(Some(env));
            Ok(rx)
        }
    }

    /// Attempts to send a message on this `Sender<A>` without blocking.
    ///
    /// This function, unlike `send`, is safe to call whether it's being
    /// called on a task or not. Note that this function, however, will *not*
    /// attempt to block the current task if the message cannot be sent.
    pub fn try_send<M>(&self, msg: M) -> Result<(), SendError<M>>
        where A: Handler<M>, <A as Actor>::Context: ToEnvelope<A>,
              M::Item: Send, M::Error: Send,
              M: ResponseType + Send + 'static,
    {
        // If the sender is currently blocked, reject the message
        if !self.poll_unparked(false).is_ready() {
            return Err(SendError::NotReady(msg))
        }

        let park_self = match self.inc_num_messages() {
            Some(park_self) => park_self,
            None => return Err(SendError::Closed(msg)),
        };

        if park_self {
            Err(SendError::NotReady(msg))
        } else {
            let env = <A::Context as ToEnvelope<A>>::pack(msg, None);
            self.queue_push_and_signal(Some(env));
            Ok(())
        }
    }

    /// Send a message on this `Sender<A>` without blocking.
    ///
    /// This function does not park current task.
    pub fn do_send<M>(&self, msg: M) -> Result<(), SendError<M>>
        where A: Handler<M>, <A as Actor>::Context: ToEnvelope<A>,
              M::Item: Send, M::Error: Send,
              M: ResponseType + Send + 'static,
    {
        if self.inc_num_messages_force(false).is_none() {
            Err(SendError::Closed(msg))
        } else {
            let env = <A::Context as ToEnvelope<A>>::pack(msg, None);
            self.queue_push_and_signal(Some(env));
            Ok(())
        }
    }

    /// While dropping the `Sender`, `task::current()` can't be called safely.
    /// In this case, in order to maintain internal consistency, a blank message
    /// is pushed onto the parked task queue.
    fn do_close(&mut self) {
        let park_self = match self.inc_num_messages_force(true) {
            Some(park_self) => park_self,
            None => return
        };

        if park_self {
            self.park(false);
        }
        self.queue_push_and_signal(None);
    }

    // Push message to the queue and signal to the receiver
    fn queue_push_and_signal(&self, msg: Option<Envelope<A>>) {
        // Push the message onto the message queue
        self.inner.message_queue.push(msg);

        // Signal to the receiver that a message has been enqueued. If the
        // receiver is parked, this will unpark the task.
        self.signal();
    }

    // Increment the number of queued messages. Returns if the sender should
    // block.
    fn inc_num_messages(&self) -> Option<bool> {
        let mut curr = self.inner.state.load(SeqCst);
        loop {
            let mut state = decode_state(curr);
            if !state.is_open {
                return None;
            }

            // receiver is full
            let park_self = self.inner.buffer != 0 && state.num_messages >= self.inner.buffer;
            if park_self {
                return Some(true);
            }

            state.num_messages += 1;

            let next = encode_state(&state);
            match self.inner.state.compare_exchange(curr, next, SeqCst, SeqCst) {
                Ok(_) => return Some(false),
                Err(actual) => curr = actual,
            }
        }
    }

    // Increment the number of queued messages. Returns if the sender should
    // block.
    fn inc_num_messages_force(&self, close: bool) -> Option<bool> {
        let mut curr = self.inner.state.load(SeqCst);

        loop {
            let mut state = decode_state(curr);
            if !state.is_open {
                return None;
            }

            state.num_messages += 1;

            // The channel is closed by all sender handles being dropped.
            if close {
                state.is_open = false;
            }

            let next = encode_state(&state);
            match self.inner.state.compare_exchange(curr, next, SeqCst, SeqCst) {
                Ok(_) => {
                    let park_self = self.inner.buffer != 0 &&
                        state.num_messages >= self.inner.buffer;
                    return Some(park_self)
                }
                Err(actual) => curr = actual,
            }
        }
    }

    // Signal to the receiver task that a message has been enqueued
    fn signal(&self) {
        // TODO
        // This logic can probably be improved by guarding the lock with an
        // atomic.
        //
        // Do this step first so that the lock is dropped when
        // `unpark` is called
        let task = {
            let mut recv_task = self.inner.recv_task.lock().unwrap();

            // If the receiver has already been unparked, then there is nothing
            // more to do
            if recv_task.unparked {
                return;
            }

            // Setting this flag enables the receiving end to detect that
            // an unpark event happened in order to avoid unnecessarily
            // parking.
            recv_task.unparked = true;
            recv_task.task.take()
        };

        if let Some(task) = task {
            task.notify();
        }
    }

    fn park(&self, can_park: bool) {
        // TODO: clean up internal state if the task::current will fail
        let task = if can_park {
            Some(task::current())
        } else {
            None
        };

        {
            let mut sender = self.sender_task.lock().unwrap();
            sender.task = task;
            sender.is_parked = true;
        }

        // Send handle over queue
        let t = Arc::clone(&self.sender_task);
        self.inner.parked_queue.push(t);

        // Check to make sure we weren't closed after we sent our task on the queue
        let state = decode_state(self.inner.state.load(SeqCst));
        self.maybe_parked.set(state.is_open);
    }

    fn poll_unparked(&self, do_park: bool) -> Async<()> {
        // First check the `maybe_parked` variable. This avoids acquiring the
        // lock in most cases
        if self.maybe_parked.get() {
            // Get a lock on the task handle
            let mut task = self.sender_task.lock().unwrap();

            if !task.is_parked {
                self.maybe_parked.set(false);
                return Async::Ready(())
            }

            // At this point, an unpark request is pending, so there will be an
            // unpark sometime in the future. We just need to make sure that
            // the correct task will be notified.
            //
            // Update the task in case the `Sender` has been moved to another
            // task
            task.task = if do_park {
                Some(task::current())
            } else {
                None
            };

            Async::NotReady
        } else {
            Async::Ready(())
        }
    }
}

impl<A: Actor> Clone for AddressSender<A> {
    fn clone(&self) -> AddressSender<A> {
        // Since this atomic op isn't actually guarding any memory and we don't
        // care about any orderings besides the ordering on the single atomic
        // variable, a relaxed ordering is acceptable.
        let mut curr = self.inner.num_senders.load(SeqCst);

        loop {
            // If the maximum number of senders has been reached, then fail
            if curr == self.inner.max_senders() {
                panic!("cannot clone `Sender` -- too many outstanding senders");
            }

            debug_assert!(curr < self.inner.max_senders());

            let next = curr + 1;
            let actual = self.inner.num_senders.compare_and_swap(curr, next, SeqCst);

            // The ABA problem doesn't matter here. We only care that the
            // number of senders never exceeds the maximum.
            if actual == curr {
                return AddressSender {
                    inner: Arc::clone(&self.inner),
                    sender_task: Arc::new(Mutex::new(SenderTask::new())),
                    maybe_parked: Cell::new(false),
                };
            }

            curr = actual;
        }
    }
}

impl<A: Actor> Drop for AddressSender<A> {
    fn drop(&mut self) {
        // Ordering between variables don't matter here
        let prev = self.inner.num_senders.fetch_sub(1, SeqCst);

        if prev == 1 {
            self.do_close();
        }
    }
}

//
//
// ===== impl Receiver =====
//
//
impl<A: Actor> AddressReceiver<A> {

    pub fn connected(&self) -> bool {
        let curr = self.inner.state.load(Relaxed);
        let state = decode_state(curr);

        state.is_open || state.num_messages != 0
    }

    pub fn sender(&mut self) -> AddressSender<A> {
        // change state to open
        let mut curr_state = self.inner.state.load(SeqCst);
        loop {
            let mut state = decode_state(curr_state);

            if state.is_open {
                break
            }
            state.is_open = true;

            let next = encode_state(&state);
            match self.inner.state.compare_exchange(curr_state, next, SeqCst, SeqCst) {
                Ok(_) => break,
                Err(actual) => curr_state = actual,
            }
        }

        // this code same as Sender::clone
        let mut curr = self.inner.num_senders.load(SeqCst);

        loop {
            // If the maximum number of senders has been reached, then fail
            if curr == self.inner.max_senders() {
                panic!("cannot clone `Sender` -- too many outstanding senders");
            }

            let next = curr + 1;
            let actual = self.inner.num_senders.compare_and_swap(curr, next, SeqCst);

            // The ABA problem doesn't matter here. We only care that the
            // number of senders never exceeds the maximum.
            if actual == curr {
                return AddressSender {
                    inner: Arc::clone(&self.inner),
                    sender_task: Arc::new(Mutex::new(SenderTask::new())),
                    maybe_parked: Cell::new(false),
                };
            }

            curr = actual;
        }
    }

    /// Closes the receiving half
    ///
    /// This prevents any further messages from being sent on the channel while
    /// still enabling the receiver to drain messages that are buffered.
    pub fn close(&mut self) {
        let mut curr = self.inner.state.load(SeqCst);

        loop {
            let mut state = decode_state(curr);

            if !state.is_open {
                break
            }

            state.is_open = false;

            let next = encode_state(&state);
            match self.inner.state.compare_exchange(curr, next, SeqCst, SeqCst) {
                Ok(_) => break,
                Err(actual) => curr = actual,
            }
        }

        // Wake up any threads waiting as they'll see that we've closed the
        // channel and will continue on their merry way.
        loop {
            match unsafe { self.inner.parked_queue.pop() } {
                PopResult::Data(task) => {
                    task.lock().unwrap().notify();
                }
                PopResult::Empty => break,
                PopResult::Inconsistent => thread::yield_now(),
            }
        }
    }

    fn next_message(&mut self) -> Async<Option<Envelope<A>>> {
        // Pop off a message
        loop {
            match unsafe { self.inner.message_queue.pop() } {
                PopResult::Data(msg) => {
                    return Async::Ready(msg);
                }
                PopResult::Empty => {
                    // The queue is empty, return NotReady
                    return Async::NotReady;
                }
                PopResult::Inconsistent => {
                    // Inconsistent means that there will be a message to pop
                    // in a short time. This branch can only be reached if
                    // values are being produced from another thread, so there
                    // are a few ways that we can deal with this:
                    //
                    // 1) Spin
                    // 2) thread::yield_now()
                    // 3) task::current().unwrap() & return NotReady
                    //
                    // For now, thread::yield_now() is used, but it would
                    // probably be better to spin a few times then yield.
                    thread::yield_now();
                }
            }
        }
    }

    // Unpark a single task handle if there is one pending in the parked queue
    fn unpark_one(&mut self) {
        loop {
            match unsafe { self.inner.parked_queue.pop() } {
                PopResult::Data(task) => {
                    task.lock().unwrap().notify();
                    return;
                }
                PopResult::Empty => {
                    // Queue empty, no task to wake up.
                    return;
                }
                PopResult::Inconsistent => {
                    // Same as above
                    thread::yield_now();
                }
            }
        }
    }

    // Try to park the receiver task
    fn try_park(&self) -> TryPark {
        let curr = self.inner.state.load(SeqCst);
        let state = decode_state(curr);

        // If the channel is closed, then there is no need to park.
        if !state.is_open && state.num_messages == 0 {
            return TryPark::Closed;
        }

        // First, track the task in the `recv_task` slot
        let mut recv_task = self.inner.recv_task.lock().unwrap();

        if recv_task.unparked {
            // Consume the `unpark` signal without actually parking
            recv_task.unparked = false;
            return TryPark::NotEmpty;
        }

        recv_task.task = Some(task::current());
        TryPark::Parked
    }

    fn dec_num_messages(&self) {
        let mut curr = self.inner.state.load(SeqCst);

        loop {
            let mut state = decode_state(curr);

            state.num_messages -= 1;

            let next = encode_state(&state);
            match self.inner.state.compare_exchange(curr, next, SeqCst, SeqCst) {
                Ok(_) => break,
                Err(actual) => curr = actual,
            }
        }
    }
}

impl<A: Actor> Stream for AddressReceiver<A> {
    type Item = Envelope<A>;
    type Error = ();

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        loop {
            // Try to read a message off of the message queue.
            let msg = match self.next_message() {
                Async::Ready(msg) => msg,
                Async::NotReady => {
                    // There are no messages to read, in this case, attempt to
                    // park. The act of parking will verify that the channel is
                    // still empty after the park operation has completed.
                    match self.try_park() {
                        TryPark::Parked => {
                            // The task was parked, and the channel is still
                            // empty, return NotReady.
                            return Ok(Async::NotReady);
                        }
                        TryPark::Closed => {
                            // The channel is closed, there will be no further
                            // messages.
                            return Ok(Async::Ready(None));
                        }
                        TryPark::NotEmpty => {
                            // A message has been sent while attempting to
                            // park. Loop again, the next iteration is
                            // guaranteed to get the message.
                            continue;
                        }
                    }
                }
            };

            // If there are any parked task handles in the parked queue, pop
            // one and unpark it.
            self.unpark_one();

            // Decrement number of messages
            self.dec_num_messages();

            // Return the message
            return Ok(Async::Ready(msg));
        }
    }
}

impl<A: Actor> Drop for AddressReceiver<A> {
    fn drop(&mut self) {
        // Drain the channel of all pending messages
        self.close();
        while self.next_message().is_ready() {
            // ...
        }
    }
}

//
//
// ===== impl Inner =====
//
//
impl<A: Actor> Inner<A> {
    // The return value is such that the total number of messages that can be
    // enqueued into the channel will never exceed MAX_CAPACITY
    fn max_senders(&self) -> usize {
        MAX_CAPACITY - self.buffer
    }
}

unsafe impl<A: Actor> Send for Inner<A> {}
unsafe impl<A: Actor> Sync for Inner<A> {}

//
//
// ===== Helpers =====
//
//
fn decode_state(num: usize) -> State {
    State {
        is_open: num & OPEN_MASK == OPEN_MASK,
        num_messages: num & MAX_CAPACITY,
    }
}

fn encode_state(state: &State) -> usize {
    let mut num = state.num_messages;

    if state.is_open {
        num |= OPEN_MASK;
    }

    num
}