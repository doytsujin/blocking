//! An executor for isolating blocking I/O in async programs.
//!
//! Sometimes there's no way to avoid blocking I/O. Consider files or stdin, which have weak async
//! support on modern operating systems. While [IOCP], [AIO], and [io_uring] are possible
//! solutions, they're not always available or ideal.
//!
//! Since blocking is not allowed inside futures, we must move blocking I/O onto a special
//! executor provided by this crate. On this executor, futures are allowed to "cheat" and block
//! without any restrictions. The executor dynamically spawns and stops threads depending on the
//! current number of running futures.
//!
//! Note that there is a limit on the number of active threads. Once that limit is hit, a running
//! task has to complete or yield before other tasks get a chance to continue running. When a
//! thread is idle, it waits for the next task or shuts down after a certain timeout.
//!
//! [IOCP]: https://en.wikipedia.org/wiki/Input/output_completion_port
//! [AIO]: http://man7.org/linux/man-pages/man2/io_submit.2.html
//! [io_uring]: https://lwn.net/Articles/776703/
//!
//! # Examples
//!
//! Spawn a blocking future with [`Blocking::spawn()`]:
//!
//! ```no_run
//! use blocking::Blocking;
//! use std::fs;
//!
//! # futures::executor::block_on(async {
//! let contents = Blocking::spawn(async { fs::read_to_string("file.txt") }).await?;
//! # std::io::Result::Ok(()) });
//! ```
//!
//! Or do the same with the [`blocking!`] macro:
//!
//! ```no_run
//! use blocking::blocking;
//! use std::fs;
//!
//! # futures::executor::block_on(async {
//! let contents = blocking!(fs::read_to_string("file.txt"))?;
//! # std::io::Result::Ok(()) });
//! ```
//!
//! Read a file and pipe its contents to stdout:
//!
//! ```no_run
//! use blocking::Blocking;
//! use std::fs::File;
//! use std::io::stdout;
//!
//! # futures::executor::block_on(async {
//! let input = Blocking::new(File::open("file.txt")?);
//! let mut output = Blocking::new(stdout());
//!
//! futures::io::copy(input, &mut output).await?;
//! # std::io::Result::Ok(()) });
//! ```
//!
//! Iterate over the contents of a directory:
//!
//! ```no_run
//! use blocking::Blocking;
//! use futures::prelude::*;
//! use std::fs;
//!
//! # futures::executor::block_on(async {
//! let mut dir = Blocking::new(fs::read_dir(".")?);
//!
//! while let Some(item) = dir.next().await {
//!     println!("{}", item?.file_name().to_string_lossy());
//! }
//! # std::io::Result::Ok(()) });
//! ```

use std::any::Any;
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::mem;
use std::panic;
use std::pin::Pin;
use std::slice;
use std::sync::atomic::{self, AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::thread;
use std::time::Duration;

use futures::channel::mpsc;
use futures::prelude::*;
use futures::task::AtomicWaker;
use once_cell::sync::Lazy;

/// A runnable future, ready for execution.
///
/// When a future is internally spawned using `async_task::spawn()` or `async_task::spawn_local()`,
/// we get back two values:
///
/// 1. an `async_task::Task<()>`, which we refer to as a `Runnable`
/// 2. an `async_task::JoinHandle<T, ()>`, which is wrapped inside a `Task<T>`
///
/// Once a `Runnable` is run, it "vanishes" and only reappears when its future is woken. When it's
/// woken up, its schedule function is called, which means the `Runnable` gets pushed into the main
/// task queue in the executor.
type Runnable = async_task::Task<()>;

struct Task<T>(Option<async_task::JoinHandle<T, ()>>);

impl<T> Drop for Task<T> {
    fn drop(&mut self) {
        if let Some(handle) = &self.0 {
            handle.cancel();
        }
    }
}

impl<T> Future for Task<T> {
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.0.as_mut().unwrap()).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(output) => Poll::Ready(output.expect("task has failed")),
        }
    }
}

/// The blocking executor.
struct Executor {
    /// Inner state of the executor.
    inner: Mutex<Inner>,

    /// Used to put idle threads to sleep and wake them up when new work comes in.
    cvar: Condvar,
}

/// Inner state of the blocking executor.
struct Inner {
    /// Number of idle threads in the pool.
    ///
    /// Idle threads are sleeping, waiting to get a task to run.
    idle_count: usize,

    /// Total number of threads in the pool.
    ///
    /// This is the number of idle threads + the number of active threads.
    thread_count: usize,

    /// The queue of blocking tasks.
    queue: VecDeque<Runnable>,
}

impl Executor {
    /// Spawns a future onto this executor.
    ///
    /// Returns a [`Task`] handle for the spawned task.
    fn spawn<T: Send + 'static>(future: impl Future<Output = T> + Send + 'static) -> Task<T> {
        static EXECUTOR: Lazy<Executor> = Lazy::new(|| Executor {
            inner: Mutex::new(Inner {
                idle_count: 0,
                thread_count: 0,
                queue: VecDeque::new(),
            }),
            cvar: Condvar::new(),
        });

        // Create a task, schedule it, and return its `Task` handle.
        let (runnable, handle) = async_task::spawn(future, |r| EXECUTOR.schedule(r), ());
        runnable.schedule();
        Task(Some(handle))
    }

    /// Runs the main loop on the current thread.
    ///
    /// This function runs blocking tasks until it becomes idle and times out.
    fn main_loop(&'static self) {
        let mut inner = self.inner.lock().unwrap();
        loop {
            // This thread is not idle anymore because it's going to run tasks.
            inner.idle_count -= 1;

            // Run tasks in the queue.
            while let Some(runnable) = inner.queue.pop_front() {
                // We have found a task - grow the pool if needed.
                self.grow_pool(inner);

                // Run the task.
                let _ = panic::catch_unwind(|| runnable.run());

                // Re-lock the inner state and continue.
                inner = self.inner.lock().unwrap();
            }

            // This thread is now becoming idle.
            inner.idle_count += 1;

            // Put the thread to sleep until another task is scheduled.
            let timeout = Duration::from_millis(500);
            let (lock, res) = self.cvar.wait_timeout(inner, timeout).unwrap();
            inner = lock;

            // If there are no tasks after a while, stop this thread.
            if res.timed_out() && inner.queue.is_empty() {
                inner.idle_count -= 1;
                inner.thread_count -= 1;
                break;
            }
        }
    }

    /// Schedules a runnable task for execution.
    fn schedule(&'static self, runnable: Runnable) {
        let mut inner = self.inner.lock().unwrap();
        inner.queue.push_back(runnable);

        // Notify a sleeping thread and spawn more threads if needed.
        self.cvar.notify_one();
        self.grow_pool(inner);
    }

    /// Spawns more blocking threads if the pool is overloaded with work.
    fn grow_pool(&'static self, mut inner: MutexGuard<'static, Inner>) {
        // If runnable tasks greatly outnumber idle threads and there aren't too many threads
        // already, then be aggressive: wake all idle threads and spawn one more thread.
        while inner.queue.len() > inner.idle_count * 5 && inner.thread_count < 500 {
            // The new thread starts in idle state.
            inner.idle_count += 1;
            inner.thread_count += 1;

            // Notify all existing idle threads because we need to hurry up.
            self.cvar.notify_all();

            // Spawn the new thread.
            thread::spawn(move || self.main_loop());
        }
    }
}

/// Spawns blocking I/O onto a thread.
///
/// Note that `blocking!(expr)` is just syntax sugar for
/// `Blocking::spawn(async move { expr }).await`.
///
/// # Examples
///
/// Read a file into a string:
///
/// ```no_run
/// use blocking::blocking;
/// use std::fs;
///
/// # futures::executor::block_on(async {
/// let contents = blocking!(fs::read_to_string("file.txt"))?;
/// # std::io::Result::Ok(()) });
/// ```
///
/// Spawn a process:
///
/// ```no_run
/// use blocking::blocking;
/// use std::process::Command;
///
/// # futures::executor::block_on(async {
/// let out = blocking!(Command::new("dir").output())?;
/// # std::io::Result::Ok(()) });
/// ```
#[macro_export]
macro_rules! blocking {
    ($($expr:tt)*) => {
        $crate::Blocking::spawn(async move { $($expr)* }).await
    };
}

/// Async I/O that runs on a thread.
///
/// This handle represents a future performing some blocking I/O on the special thread pool. The
/// output of the future can be awaited because [`Blocking`] itself is a future.
///
/// It's also possible to interact with [`Blocking`] through [`Stream`], [`AsyncRead`] and
/// [`AsyncWrite`] traits if the inner type implements [`Iterator`], [`Read`], or [`Write`].
///
/// To spawn a future and start it immediately, use [`Blocking::spawn()`]. To create an I/O handle
/// that will lazily spawn an I/O future on its own, use [`Blocking::new()`].
///
/// If the [`Blocking`] handle is dropped, the future performing I/O will be canceled if it hasn't
/// completed yet. However, note that it's not possible to forcibly cancel blocking I/O, so if the
/// future is currently running, it won't be canceled until it yields.
///
/// If writing some data through the [`AsyncWrite`] trait, make sure to flush before dropping the
/// [`Blocking`] handle or some written data might get lost. Alternatively, await the handle to
/// complete the pending work and extract the inner blocking I/O handle.
///
/// # Examples
///
/// ```
/// use blocking::Blocking;
/// use futures::prelude::*;
/// use std::io::stdout;
///
/// # futures::executor::block_on(async {
/// let mut stdout = Blocking::new(stdout());
/// stdout.write_all(b"Hello world!").await?;
///
/// let inner = stdout.await;
/// # std::io::Result::Ok(()) });
/// ```
pub struct Blocking<T>(State<T>);

impl<T> Blocking<T> {
    /// Wraps a blocking I/O handle into an async interface.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use blocking::Blocking;
    /// use std::io::stdin;
    ///
    /// # futures::executor::block_on(async {
    /// // Create an async handle to standard input.
    /// let stdin = Blocking::new(stdin());
    /// # std::io::Result::Ok(()) });
    /// ```
    pub fn new(io: T) -> Blocking<T> {
        Blocking(State::Idle(Some(Box::new(io))))
    }

    /// Gets a mutable reference to the blocking I/O handle.
    ///
    /// This is an async method because the I/O handle might be on a different thread and needs to
    /// be moved onto the current thread before we can get a reference to it.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use blocking::Blocking;
    /// use std::fs::File;
    ///
    /// # futures::executor::block_on(async {
    /// let mut file = Blocking::new(File::create("file.txt")?);
    /// let metadata = file.get_mut().await.metadata()?;
    /// # std::io::Result::Ok(()) });
    /// ```
    pub async fn get_mut(&mut self) -> &mut T {
        // Wait for the running task to stop and ignore I/O errors if there are any.
        let _ = future::poll_fn(|cx| self.poll_stop(cx)).await;

        // Assume idle state and get a reference to the inner value.
        match &mut self.0 {
            State::Idle(t) => t.as_mut().expect("inner value was taken out"),
            State::Streaming(..) | State::Reading(..) | State::Writing(..) | State::Task(..) => {
                unreachable!("when stopped, the state machine must be in idle state");
            }
        }
    }

    /// Extracts the inner blocking I/O handle.
    ///
    /// This is an async method because the I/O handle might be on a different thread and needs to
    /// be moved onto the current thread before we can extract it.
    ///
    /// Note that awaiting this method is equivalent to awaiting the [`Blocking`] handle.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use blocking::Blocking;
    /// use futures::prelude::*;
    /// use std::fs::File;
    ///
    /// # futures::executor::block_on(async {
    /// let mut file = Blocking::new(File::create("file.txt")?);
    /// file.write_all(b"Hello world!").await?;
    ///
    /// let file = file.into_inner().await;
    /// # std::io::Result::Ok(()) });
    /// ```
    pub async fn into_inner(self) -> T {
        // There's a bug in rustdoc causing it to render `mut self` as `__arg0: Self`, so we just
        // bind `self` to a local mutable variable.
        let mut this = self;

        // Wait for the running task to stop and ignore I/O errors if there are any.
        let _ = future::poll_fn(|cx| this.poll_stop(cx)).await;

        // Assume idle state and extract the inner value.
        match &mut this.0 {
            State::Idle(t) => *t.take().expect("inner value was taken out"),
            State::Streaming(..) | State::Reading(..) | State::Writing(..) | State::Task(..) => {
                unreachable!("when stopped, the state machine must be in idle state");
            }
        }
    }

    /// Waits for the running task to stop.
    ///
    /// On success, the state machine is moved into the idle state.
    fn poll_stop(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        loop {
            match &mut self.0 {
                State::Idle(_) => return Poll::Ready(Ok(())),

                State::Streaming(any, task) => {
                    // Drop the receiver to close the channel. This stops the `send()` operation in
                    // the task, after which the task returns the iterator back.
                    any.take();

                    // Poll the task to retrieve the iterator.
                    let iter = futures::ready!(Pin::new(task).poll(cx));
                    self.0 = State::Idle(Some(iter));
                }

                State::Reading(reader, task) => {
                    // Drop the reader to close the pipe. This stops the `futures::io::copy`
                    // operation in the task, after which the task returns the I/O handle back.
                    reader.take();

                    // Poll the task to retrieve the I/O handle.
                    let (res, io) = futures::ready!(Pin::new(task).poll(cx));
                    // Make sure to move into the idle state before reporting errors.
                    self.0 = State::Idle(Some(io));
                    res?;
                }

                State::Writing(writer, task) => {
                    // Drop the writer to close the pipe. This stops the `futures::io::copy`
                    // operation in the task, after which the task flushes the I/O handle and
                    // returns it back.
                    writer.take();

                    // Poll the task to retrieve the I/O handle.
                    let (res, io) = futures::ready!(Pin::new(task).poll(cx));
                    // Make sure to move into the idle state before reporting errors.
                    self.0 = State::Idle(Some(io));
                    res?;
                }

                State::Task(task) => {
                    // Poll the task to retrieve the inner value.
                    let t = futures::ready!(Pin::new(task).poll(cx));
                    self.0 = State::Idle(Some(Box::new(t)));
                }
            }
        }
    }
}

impl<T: Send + 'static> Blocking<T> {
    /// Spawns a future that is allowed to do blocking I/O.
    ///
    /// If the [`Blocking`] handle is dropped, the future will be canceled if it hasn't completed
    /// yet. However, note that it's not possible to forcibly cancel blocking I/O, so if the future
    /// is currently running, it won't be canceled until it yields.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use blocking::Blocking;
    /// use std::fs;
    ///
    /// # futures::executor::block_on(async {
    /// let contents = Blocking::spawn(async { fs::read_to_string("file.txt") }).await?;
    /// # std::io::Result::Ok(()) });
    /// ```
    pub fn spawn(future: impl Future<Output = T> + Send + 'static) -> Blocking<T> {
        let task = Executor::spawn(future);
        Blocking(State::Task(task))
    }
}

impl<T> Future for Blocking<T> {
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Wait for the running task to stop and ignore I/O errors if there are any.
        let _ = futures::ready!(self.poll_stop(cx));

        // Assume idle state and extract the inner value.
        match &mut self.0 {
            State::Idle(t) => Poll::Ready(*t.take().expect("inner value was taken out")),
            State::Streaming(..) | State::Reading(..) | State::Writing(..) | State::Task(..) => {
                unreachable!("when stopped, the state machine must be in idle state");
            }
        }
    }
}

/// Current state of a blocking task.
enum State<T> {
    /// There is no blocking task.
    ///
    /// The inner value is readily available, unless it has already been extracted. The value is
    /// extracted out by [`Blocking::into_inner()`], [`AsyncWrite::poll_close()`], or by awaiting
    /// [`Blocking`].
    Idle(Option<Box<T>>),

    /// A task was spawned by [`Blocking::spawn()`] and is still running.
    Task(Task<T>),

    /// The inner value is an [`Iterator`] currently iterating in a task.
    ///
    /// The `dyn Any` value here is a `mpsc::Receiver<<T as Iterator>::Item>`.
    Streaming(Option<Box<dyn Any>>, Task<Box<T>>),

    /// The inner value is a [`Read`] currently reading in a task.
    Reading(Option<Reader>, Task<(io::Result<()>, Box<T>)>),

    /// The inner value is a [`Write`] currently writing in a task.
    Writing(Option<Writer>, Task<(io::Result<()>, Box<T>)>),
}

impl<T: Iterator + Send + 'static> Stream for Blocking<T>
where
    T::Item: Send + 'static,
{
    type Item = T::Item;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T::Item>> {
        loop {
            match &mut self.0 {
                // If not in idle or active streaming state, stop the running task.
                State::Task(..)
                | State::Streaming(None, _)
                | State::Reading(..)
                | State::Writing(..) => {
                    // Wait for the running task to stop.
                    let _ = futures::ready!(self.poll_stop(cx));
                }

                // If idle, start a streaming task.
                State::Idle(iter) => {
                    // If idle, take the iterator out to run it on a blocking task.
                    let mut iter = iter.take().unwrap();

                    // This channel capacity seems to work well in practice. If it's too low, there
                    // will be too much synchronization between tasks. If too high, memory
                    // consumption increases.
                    let (mut sender, receiver) = mpsc::channel(8 * 1024); // 8192 items

                    // Spawn a blocking task that runs the iterator and returns it when done.
                    let task = Executor::spawn(async move {
                        for item in &mut iter {
                            if sender.send(item).await.is_err() {
                                break;
                            }
                        }
                        iter
                    });

                    // Move into the busy state and poll again.
                    self.0 = State::Streaming(Some(Box::new(receiver)), task);
                }

                // If streaming, receive an item.
                State::Streaming(Some(any), task) => {
                    let receiver = any.downcast_mut::<mpsc::Receiver<T::Item>>().unwrap();

                    // Poll the channel.
                    let opt = futures::ready!(Pin::new(receiver).poll_next(cx));

                    // If the channel is closed, retrieve the iterator back from the blocking task.
                    // This is not really a required step, but it's cleaner to drop the iterator on
                    // the same thread that created it.
                    if opt.is_none() {
                        // Poll the task to retrieve the iterator.
                        let iter = futures::ready!(Pin::new(task).poll(cx));
                        self.0 = State::Idle(Some(iter));
                    }

                    return Poll::Ready(opt);
                }
            }
        }
    }
}

impl<T: Read + Send + 'static> AsyncRead for Blocking<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            match &mut self.0 {
                // If not in idle or active reading state, stop the running task.
                State::Task(..)
                | State::Reading(None, _)
                | State::Streaming(..)
                | State::Writing(..) => {
                    // Wait for the running task to stop.
                    futures::ready!(self.poll_stop(cx))?;
                }

                // If idle, start a reading task.
                State::Idle(io) => {
                    // If idle, take the I/O handle out to read it on a blocking task.
                    let mut io = io.take().unwrap();

                    // This pipe capacity seems to work well in practice. If it's too low, there
                    // will be too much synchronization between tasks. If too high, memory
                    // consumption increases.
                    let (reader, mut writer) = pipe(8 * 1024 * 1024); // 8 MB

                    // Spawn a blocking task that reads and returns the I/O handle when done.
                    let task = Executor::spawn(async move {
                        // Copy bytes from the I/O handle into the pipe until the pipe is closed or
                        // an error occurs.
                        loop {
                            match future::poll_fn(|cx| writer.poll_write(cx, &mut io)).await {
                                Ok(0) => return (Ok(()), io),
                                Ok(_) => {}
                                Err(err) => return (Err(err), io),
                            }
                        }
                    });

                    // Move into the busy state and poll again.
                    self.0 = State::Reading(Some(reader), task);
                }

                // If reading, read bytes from the pipe.
                State::Reading(Some(reader), task) => {
                    // Poll the pipe.
                    let n = futures::ready!(Pin::new(reader).poll_read(cx, buf))?;

                    // If the pipe is closed, retrieve the I/O handle back from the blocking task.
                    // This is not really a required step, but it's cleaner to drop the handle on
                    // the same thread that created it.
                    if n == 0 {
                        // Poll the task to retrieve the I/O handle.
                        let (res, io) = futures::ready!(Pin::new(task).poll(cx));
                        // Make sure to move into the idle state before reporting errors.
                        self.0 = State::Idle(Some(io));
                        res?;
                    }

                    return Poll::Ready(Ok(n));
                }
            }
        }
    }
}

impl<T: Write + Send + 'static> AsyncWrite for Blocking<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            match &mut self.0 {
                // If not in idle or active writing state, stop the running task.
                State::Task(..)
                | State::Writing(None, _)
                | State::Streaming(..)
                | State::Reading(..) => {
                    // Wait for the running task to stop.
                    futures::ready!(self.poll_stop(cx))?;
                }

                // If idle, start the writing task.
                State::Idle(io) => {
                    // If idle, take the I/O handle out to write on a blocking task.
                    let mut io = io.take().unwrap();

                    // This pipe capacity seems to work well in practice. If it's too low, there will
                    // be too much synchronization between tasks. If too high, memory consumption
                    // increases.
                    let (mut reader, writer) = pipe(8 * 1024 * 1024); // 8 MB

                    // Spawn a blocking task that writes and returns the I/O handle when done.
                    let task = Executor::spawn(async move {
                        // Copy bytes from the pipe into the I/O handle until the pipe is closed or an
                        // error occurs. Flush the I/O handle at the end.
                        loop {
                            match future::poll_fn(|cx| reader.poll_read(cx, &mut io)).await {
                                Ok(0) => return (io.flush(), io),
                                Ok(_) => {}
                                Err(err) => {
                                    let _ = io.flush();
                                    return (Err(err), io);
                                }
                            }
                        }
                    });

                    // Move into the busy state.
                    self.0 = State::Writing(Some(writer), task);
                }

                // If writing,write more bytes into the pipe.
                State::Writing(Some(writer), _) => return Pin::new(writer).poll_write(cx, buf),
            }
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        loop {
            match &mut self.0 {
                // If not in idle state, stop the running task.
                State::Task(..)
                | State::Streaming(..)
                | State::Writing(..)
                | State::Reading(..) => {
                    // Wait for the running task to stop.
                    futures::ready!(self.poll_stop(cx))?;
                }

                // Idle implies flushed.
                State::Idle(_) => return Poll::Ready(Ok(())),
            }
        }
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // First, make sure the I/O handle is flushed.
        futures::ready!(Pin::new(&mut *self).poll_flush(cx))?;

        // Then move into the idle state with no I/O handle, thus dropping it.
        self.0 = State::Idle(None);
        Poll::Ready(Ok(()))
    }
}

/// Creates a bounded single-producer single-consumer pipe.
///
/// A pipe is a ring buffer of `cap` bytes that implements traits [`AsyncRead`] and [`AsyncWrite`].
///
/// When the sender is dropped, remaining bytes in the pipe can still be read. After that, attempts
/// to read will result in `Ok(0)`, i.e. they will always 'successfully' read 0 bytes.
///
/// When the receiver is dropped, the pipe is closed and no more bytes and be written into it.
/// Further writes will result in `Ok(0)`, i.e. they will always 'successfully' write 0 bytes.
fn pipe(cap: usize) -> (Reader, Writer) {
    assert!(cap > 0, "capacity must be positive");
    assert!(cap.checked_mul(2).is_some(), "capacity is too large");

    // Allocate the ring buffer.
    let mut v = Vec::with_capacity(cap);
    let buffer = v.as_mut_ptr();
    mem::forget(v);

    let inner = Arc::new(Pipe {
        head: AtomicUsize::new(0),
        tail: AtomicUsize::new(0),
        reader: AtomicWaker::new(),
        writer: AtomicWaker::new(),
        closed: AtomicBool::new(false),
        buffer,
        cap,
    });

    let r = Reader {
        inner: inner.clone(),
        head: 0,
        tail: 0,
    };

    let w = Writer {
        inner,
        head: 0,
        tail: 0,
        zeroed_until: 0,
    };

    (r, w)
}

/// The reading side of a pipe.
#[derive(Debug)]
struct Reader {
    /// The inner ring buffer.
    inner: Arc<Pipe>,

    /// The head index, moved by the reader, in the range `0..2*cap`.
    ///
    /// This index always matches `inner.head`.
    head: usize,

    /// The tail index, moved by the writer, in the range `0..2*cap`.
    ///
    /// This index is a snapshot of `index.tail` that might become stale at any point.
    tail: usize,
}

/// The writing side of a pipe.
#[derive(Debug)]
struct Writer {
    /// The inner ring buffer.
    inner: Arc<Pipe>,

    /// The head index, moved by the reader, in the range `0..2*cap`.
    ///
    /// This index is a snapshot of `index.head` that might become stale at any point.
    head: usize,

    /// The tail index, moved by the writer, in the range `0..2*cap`.
    ///
    /// This index always matches `inner.tail`.
    tail: usize,

    /// How many bytes at the beginning of the buffer have been zeroed.
    ///
    /// The pipe allocates an uninitialized buffer, and we must be careful about passing
    /// uninitialized data to user code. Zeroing the buffer right after allocation would be too
    /// expensive, so we zero it in smaller chunks as the writer makes progress.
    zeroed_until: usize,
}

unsafe impl Send for Reader {}
unsafe impl Send for Writer {}

/// The inner ring buffer.
///
/// Head and tail indices are in the range `0..2*cap`, even though they really map onto the
/// `0..cap` range. The distance between head and tail indices is never more than `cap`.
///
/// The reason why indices are not in the range `0..cap` is because we need to distinguish between
/// the pipe being empty and being full. If head and tail were in `0..cap`, then `head == tail`
/// could mean the pipe is either empty or full, but we don't know which!
#[derive(Debug)]
struct Pipe {
    /// The head index, moved by the reader, in the range `0..2*cap`.
    head: AtomicUsize,

    /// The tail index, moved by the writer, in the range `0..2*cap`.
    tail: AtomicUsize,

    /// A waker representing the blocked reader.
    reader: AtomicWaker,

    /// A waker representing the blocked writer.
    writer: AtomicWaker,

    /// Set to `true` if the reader or writer was dropped.
    closed: AtomicBool,

    /// The byte buffer.
    buffer: *mut u8,

    /// The buffer capacity.
    cap: usize,
}

impl Drop for Pipe {
    fn drop(&mut self) {
        // Deallocate the byte buffer.
        unsafe {
            Vec::from_raw_parts(self.buffer, 0, self.cap);
        }
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        // Dropping closes the pipe and then wakes the writer.
        self.inner.closed.store(true, Ordering::SeqCst);
        self.inner.writer.wake();
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        // Dropping closes the pipe and then wakes the reader.
        self.inner.closed.store(true, Ordering::SeqCst);
        self.inner.reader.wake();
    }
}

impl Reader {
    fn poll_read(&mut self, cx: &mut Context<'_>, mut dest: impl Write) -> Poll<io::Result<usize>> {
        let cap = self.inner.cap;

        // Calculates the distance between two indices.
        let distance = |a: usize, b: usize| {
            if a <= b {
                b - a
            } else {
                2 * cap - (a - b)
            }
        };

        // If the pipe appears to be empty...
        if distance(self.head, self.tail) == 0 {
            // Reload the tail in case it's become stale.
            self.tail = self.inner.tail.load(Ordering::Acquire);

            // If the pipe is now really empty...
            if distance(self.head, self.tail) == 0 {
                // Register the waker.
                self.inner.reader.register(cx.waker());
                atomic::fence(Ordering::SeqCst);

                // Reload the tail after registering the waker.
                self.tail = self.inner.tail.load(Ordering::Acquire);

                // If the pipe is still empty...
                if distance(self.head, self.tail) == 0 {
                    // Check whether the pipe is closed or just empty.
                    if self.inner.closed.load(Ordering::Relaxed) {
                        return Poll::Ready(Ok(0));
                    } else {
                        return Poll::Pending;
                    }
                }
            }
        }

        // The pipe is not empty so remove the waker.
        self.inner.reader.take();

        // Given an index in `0..2*cap`, returns the real index in `0..cap`.
        let real_index = |i: usize| {
            if i < cap {
                i
            } else {
                i - cap
            }
        };

        // Number of bytes read so far.
        let mut count = 0;

        loop {
            // Calculate how many bytes to read in this iteration.
            let n = (128 * 1024) // Not too many bytes in one go - better to wake the writer soon!
                .min(distance(self.head, self.tail)) // No more than bytes in the pipe.
                .min(cap - real_index(self.head)); // Don't go past the buffer boundary.

            // Create a slice of data in the pipe buffer.
            let pipe_slice =
                unsafe { slice::from_raw_parts(self.inner.buffer.add(real_index(self.head)), n) };

            // Copy bytes from the pipe buffer into `dest`.
            let n = dest
                .write(pipe_slice)
                .expect("shouldn't fail because `dest` is a slice");
            count += n;

            // If pipe is empty or `dest` is full, return.
            if n == 0 {
                return Poll::Ready(Ok(count));
            }

            // Move the head forward.
            if self.head + n < 2 * cap {
                self.head += n;
            } else {
                self.head = 0;
            }

            // Store the current head index.
            self.inner.head.store(self.head, Ordering::Release);

            // Wake the writer because the pipe is not full.
            self.inner.writer.wake();
        }
    }
}

impl Writer {
    fn poll_write(&mut self, cx: &mut Context<'_>, mut src: impl Read) -> Poll<io::Result<usize>> {
        // Just a quick check if the pipe is closed, which is why a relaxed load is okay.
        if self.inner.closed.load(Ordering::Relaxed) {
            return Poll::Ready(Ok(0));
        }

        // Calculates the distance between two indices.
        let cap = self.inner.cap;
        let distance = |a: usize, b: usize| {
            if a <= b {
                b - a
            } else {
                2 * cap - (a - b)
            }
        };

        // If the pipe appears to be full...
        if distance(self.head, self.tail) == cap {
            // Reload the head in case it's become stale.
            self.head = self.inner.head.load(Ordering::Acquire);

            // If the pipe is now really empty...
            if distance(self.head, self.tail) == cap {
                // Register the waker.
                self.inner.writer.register(cx.waker());
                atomic::fence(Ordering::SeqCst);

                // Reload the head after registering the waker.
                self.head = self.inner.head.load(Ordering::Acquire);

                // If the pipe is still full...
                if distance(self.head, self.tail) == cap {
                    // Check whether the pipe is closed or just full.
                    if self.inner.closed.load(Ordering::Relaxed) {
                        return Poll::Ready(Ok(0));
                    } else {
                        return Poll::Pending;
                    }
                }
            }
        }

        // The pipe is not full so remove the waker.
        self.inner.writer.take();

        // Given an index in `0..2*cap`, returns the real index in `0..cap`.
        let real_index = |i: usize| {
            if i < cap {
                i
            } else {
                i - cap
            }
        };

        // Number of bytes written so far.
        let mut count = 0;

        loop {
            // Calculate how many bytes to write in this iteration.
            let n = (128 * 1024) // Not too many bytes in one go - better to wake the reader soon!
                .min(self.zeroed_until * 2 + 4096) // Don't zero too many bytes when starting.
                .min(cap - distance(self.head, self.tail)) // No more than space in the pipe.
                .min(cap - real_index(self.tail)); // Don't go past the buffer boundary.

            // Create a slice of available space in the pipe buffer.
            let pipe_slice_mut = unsafe {
                let from = real_index(self.tail);
                let to = from + n;

                // Make sure all bytes in the slice are initialized.
                if self.zeroed_until < to {
                    self.inner
                        .buffer
                        .add(self.zeroed_until)
                        .write_bytes(0u8, to - self.zeroed_until);
                    self.zeroed_until = to;
                }

                slice::from_raw_parts_mut(self.inner.buffer.add(from), n)
            };

            // Copy bytes from `src` into the piper buffer.
            let n = src
                .read(pipe_slice_mut)
                .expect("shouldn't fail because `src` is a slice");
            count += n;

            // If the pipe is full or `src` is empty, return.
            if n == 0 {
                return Poll::Ready(Ok(count));
            }

            // Move the tail forward.
            if self.tail + n < 2 * cap {
                self.tail += n;
            } else {
                self.tail = 0;
            }

            // Store the current tail index.
            self.inner.tail.store(self.tail, Ordering::Release);

            // Wake the reader because the pipe is not empty.
            self.inner.reader.wake();
        }
    }
}
