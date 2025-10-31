//! Timer implementation using Windows high-res timers and I/O completion ports.
//!
//! A background thread monitors a single, global completion port for timer events.
//! When an event occurs, it notifies the corresponding async task, waking it to be polled and deliver readiness.
//!
//! Drawing from the [Go implementation](https://devblogs.microsoft.com/go/high-resolution-timers-windows/), low-level NT APIs are used to associate timers
//! with the completion port, enabling sub-millisecond resolution.

use parking_lot::Mutex;
use std::io::Result;
use std::os::raw::c_void;
use std::pin::Pin;
use std::ptr;
use std::sync::{Arc, OnceLock, Weak};
use std::task::{Context, Poll, Waker};
use std::thread::JoinHandle;
use std::time::Duration;
use windows::core::Owned;

use windows::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows::Win32::Foundation::{
    ERROR_ABANDONED_WAIT_0, GENERIC_ACCESS_RIGHTS, GENERIC_ALL, HANDLE, INVALID_HANDLE_VALUE,
    NTSTATUS,
};
use windows::Win32::System::IO::{
    CreateIoCompletionPort, GetQueuedCompletionStatusEx, OVERLAPPED, OVERLAPPED_ENTRY,
    PostQueuedCompletionStatus,
};
use windows::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CancelWaitableTimer, CreateWaitableTimerExW,
    SYNCHRONIZATION_SYNCHRONIZE, SetWaitableTimer, TIMER_MODIFY_STATE, TIMER_QUERY_STATE,
};

// Publicly documented NT APIs.
#[link(name = "ntdll")]
unsafe extern "system" {
    /// <https://learn.microsoft.com/en-us/windows/win32/devnotes/ntassociatewaitcompletionpacket>
    unsafe fn NtAssociateWaitCompletionPacket(
        WaitCompletionPacketHandle: HANDLE,
        IoCompletionHandle: HANDLE,
        TargetObjectHandle: HANDLE,
        // Returned as OVERLAPPED_ENTRY::lpCompletionKey
        KeyContext: *const c_void,
        // Returned as OVERLAPPED_ENTRY::lpOverlapped
        ApcContext: *const c_void,
        IoStatus: NTSTATUS,
        IoStatusInformation: usize,
        AlreadySignaled: *mut u8,
    ) -> NTSTATUS;

    /// <https://learn.microsoft.com/en-us/windows/win32/devnotes/ntcancelwaitcompletionpacket>
    unsafe fn NtCancelWaitCompletionPacket(
        WaitCompletionPacketHandle: HANDLE,
        RemoveSignaledPacket: u8,
    ) -> NTSTATUS;

    /// <https://learn.microsoft.com/en-us/windows/win32/devnotes/ntcreatewaitcompletionpacket>
    unsafe fn NtCreateWaitCompletionPacket(
        WaitCompletionPacketHandle: *mut HANDLE,
        DesiredAccess: GENERIC_ACCESS_RIGHTS,
        ObjectAttributes: *const OBJECT_ATTRIBUTES,
    ) -> NTSTATUS;
}

/// Shared state for each timer.
#[derive(Debug)]
struct SharedTimerState {
    timer: Owned<HANDLE>,
    waker: Option<Waker>,
    fired_counter: Option<usize>,
    wait_completion_packet: Option<Owned<HANDLE>>,
}

/// SAFETY: Completion packets and timers in a mutex can safely be shared.
unsafe impl Send for SharedTimerState {}

impl SharedTimerState {
    fn new(timer: Owned<HANDLE>) -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            timer,
            // Not polled yet, so no waker
            waker: None,
            // Never fired
            fired_counter: None,
            // Not intialized
            wait_completion_packet: None,
        }))
    }

    /// Wakes up the associated task this timer was polled in, if any.
    fn wake(&self) {
        if let Some(waker) = &self.waker {
            waker.wake_by_ref();
        }
    }

    /// Marks that this timer has fired. Ensures that any task that awaits this timer after it fired will still see the fire event.
    fn fired(&mut self) {
        self.fired_counter = Some(self.fired_counter.map_or(0, |c| c + 1));
    }
}

struct CompletionPort(Owned<HANDLE>);

/// SAFETY: A completion port can safely be concurrently accessed.
unsafe impl Send for CompletionPort {}
/// SAFETY: A completion port can safely be concurrently accessed.
unsafe impl Sync for CompletionPort {}

/// Internal state of the background thread responsible for notifying when timers have fired.
struct BackgroundTimerThread {
    completion_port: Arc<CompletionPort>,
    /// Associated thread.
    _handle: JoinHandle<()>,
}

impl BackgroundTimerThread {
    /// Default size chosen based on the Go implementation.
    ///
    /// <https://go.dev/src/runtime/netpoll_windows.go>
    const OVERLAPPED_ENTRY_BUFFER_SIZE: usize = 64;
    /// Number of threads expected to concurrently access the completion port created with [`CreateIoCompletionPort`].
    ///
    /// It is ideally at most 2, given that the background thread is always polling (+1) and timer creation/drop is fast (+1).
    const NUM_THREADS: u32 = 2;
    /// A value for [OVERLAPPED::lpCompletionKey] indicating the boxed weak pointer in OVERLAPPED::lpOverlapped can now be dropped.
    ///
    /// This is necessary because [`NtCancelWaitCompletionPacket`] can incorrectly return success instead of cancelled,
    /// causing a use after free in the [`Timer`] drop implementation when we tried to free lpOverlapped.
    const DROP_KEY: usize = 0xDEAD;

    /// Get or instantiate the background thread.
    fn get() -> &'static Self {
        static REACTOR: OnceLock<BackgroundTimerThread> = OnceLock::new();
        REACTOR.get_or_init(|| {
            let completion_port = Arc::new(CompletionPort(unsafe {
                Owned::new(
                    CreateIoCompletionPort(INVALID_HANDLE_VALUE, None, 0, Self::NUM_THREADS)
                        .expect("Failed to create global completion port"),
                )
            }));

            let completion_port_for_thread = completion_port.clone();

            let handle = std::thread::spawn(move || {
                Self::run(completion_port_for_thread);
            });

            Self {
                completion_port,
                _handle: handle,
            }
        })
    }

    /// Starts an infinite loop that repeatedly calls [`GetQueuedCompletionStatusEx`] and processes any returned entries.
    fn run(completion_port: Arc<CompletionPort>) {
        let mut entries = [OVERLAPPED_ENTRY::default(); Self::OVERLAPPED_ENTRY_BUFFER_SIZE];

        loop {
            let mut num_entries = 0;
            // Block indefinitely waiting for timer completions
            let result = unsafe {
                GetQueuedCompletionStatusEx(
                    *completion_port.0,
                    &mut entries,
                    &mut num_entries,
                    u32::MAX,
                    false,
                )
            };

            match result {
                Ok(()) => {}
                // Completion port was closed unexpectedly
                Err(err) if err.code() == ERROR_ABANDONED_WAIT_0.to_hresult() => {
                    break;
                }
                Err(_other) => continue,
            }

            // Process completed timers
            for entry in &entries[..num_entries as usize] {
                if entry.lpOverlapped.is_null() {
                    // Spurious wakeup?
                    continue;
                }
                let weak: Box<Weak<Mutex<SharedTimerState>>> = unsafe {
                    Box::from_raw(entry.lpOverlapped as *mut Weak<Mutex<SharedTimerState>>)
                };

                if entry.lpCompletionKey == Self::DROP_KEY {
                    // "free this memory" wakeup from Timer drop impl.
                    drop(weak);
                } else if let Some(state) = weak.upgrade() {
                    let mut state = state.lock();
                    state.fired();
                    state.wake();

                    if let Some(wait_completion_packet) = state.wait_completion_packet.as_ref() {
                        let mut signaled = 0;
                        // TODO: log this failure?
                        let _ = unsafe {
                            NtAssociateWaitCompletionPacket(
                                **wait_completion_packet,
                                *completion_port.0,
                                *state.timer,
                                ptr::null(),
                                entry.lpOverlapped as *const c_void,
                                NTSTATUS::default(),
                                0,
                                &mut signaled,
                            )
                        }
                        .ok();

                        // Timer already triggered. Don't wake because we still have the lock so
                        // any previous poll can't complete until this is done.
                        if signaled != 0 {
                            state.fired();
                        }

                        // Don't drop weak until we get DROP_KEY
                        let _ = Box::into_raw(weak);
                    } else {
                        // Timer is gone (raced between upgrade & lock).
                        //
                        // Don't drop weak until we get DROP_KEY
                        let _ = Box::into_raw(weak);
                    }
                    drop(state);
                } else {
                    // Timer is gone (arc dropped but haven't received the entry indicating to free lpOverlapped yet).
                    //
                    // Don't drop weak until we get DROP_KEY
                    let _ = Box::into_raw(weak);
                }
            }
        }
    }

    fn completion_port(&self) -> &CompletionPort {
        &self.completion_port
    }
}

/// Create a new wait completion packet not associated with anything.
fn new_wait_completion_packet() -> windows::core::Result<Owned<HANDLE>> {
    let mut handle = HANDLE::default();
    unsafe { NtCreateWaitCompletionPacket(&mut handle, GENERIC_ALL, ptr::null()) }.ok()?;
    Ok(unsafe { Owned::new(handle) })
}

/// Add this timer to the completion port so it will fire on the background thread.
///
/// Returns lpOverlapped and whether the timer has already fired.
fn enqueue_timer(
    state: &Arc<Mutex<SharedTimerState>>,
) -> windows::core::Result<(*mut Weak<Mutex<SharedTimerState>>, bool)> {
    let wait_completion_packet = new_wait_completion_packet()?;
    let apc_context = Box::into_raw(Box::new(Arc::downgrade(state)));

    let mut state = state.lock();
    debug_assert!(state.wait_completion_packet.is_none());
    let wait_completion_packet = state.wait_completion_packet.insert(wait_completion_packet);
    let completion_port = BackgroundTimerThread::get().completion_port();
    let key_ctx = ptr::null();
    let mut signaled = 0u8;

    let res = unsafe {
        NtAssociateWaitCompletionPacket(
            **wait_completion_packet,
            *completion_port.0,
            *state.timer,
            key_ctx,
            apc_context as *const c_void,
            NTSTATUS::default(),
            0,
            &mut signaled,
        )
    }
    .ok();

    if let Err(err) = res {
        // Free the context since we got an error.
        unsafe {
            let _ = Box::from_raw(apc_context);
        }
        return Err(err);
    }

    Ok((apc_context, signaled != 0))
}

fn new_timer() -> windows::core::Result<Owned<HANDLE>> {
    unsafe {
        Ok(Owned::new(CreateWaitableTimerExW(
            None,
            None,
            // TODO: check for high res timer support on this system
            CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
            TIMER_MODIFY_STATE.0 | TIMER_QUERY_STATE.0 | SYNCHRONIZATION_SYNCHRONIZE.0,
        )?))
    }
}

fn set_timer(
    timer: &HANDLE,
    deadline: Duration,
    period: Option<Duration>,
) -> windows::core::Result<()> {
    // Negative time is relative
    let duetime = -((deadline.as_nanos() / 100) as i64);
    unsafe {
        SetWaitableTimer(
            *timer,
            &duetime,
            period.map(|p| p.as_millis() as i32).unwrap_or(0),
            None,
            None,
            false,
        )
    }
}

#[derive(Debug)]
struct Timer {
    state: Arc<Mutex<SharedTimerState>>,
    /// Used to tell the background thread to free lpOverlapped when [`Timer`] is dropped.
    weak_state_ptr: *mut Weak<Mutex<SharedTimerState>>,
    /// Counter used to distinguish when the timer has actually fired in [`SharedTimerState::fired_counter`].
    last_fired: Option<usize>,
}

impl Timer {
    fn new() -> Result<Self> {
        let timer = new_timer().map_err(|e| std::io::Error::from_raw_os_error(e.code().0))?;
        let state = SharedTimerState::new(timer);
        let (weak_state_ptr, already_signaled) =
            enqueue_timer(&state).map_err(|e| std::io::Error::from_raw_os_error(e.code().0))?;
        if already_signaled {
            state.lock().fired();
        }
        Ok(Self {
            state,
            weak_state_ptr,
            last_fired: None,
        })
    }

    fn sleep(&mut self, duration: Duration) -> Result<()> {
        set_timer(&self.state.lock().timer, duration, None)
            .map_err(|e| std::io::Error::from_raw_os_error(e.code().0))
    }

    fn interval(&mut self, period: Duration) -> Result<()> {
        set_timer(&self.state.lock().timer, period, Some(period))
            .map_err(|e| std::io::Error::from_raw_os_error(e.code().0))
    }

    fn interval_at(&mut self, delay_until_start: Duration, period: Duration) -> Result<()> {
        set_timer(&self.state.lock().timer, delay_until_start, Some(period))
            .map_err(|e| std::io::Error::from_raw_os_error(e.code().0))
    }

    /// Cancels any outstanding triggers.
    fn clear(&mut self) -> Result<()> {
        let mut state = self.state.lock();

        // Best-effort stop the timer from firing. Even if it does fire,
        // we shouldn't get a wakeup because the waker was cleared.
        //
        // TODO: we should likely store a generation counter to skip wakeups from before a reset.
        let _ = unsafe { CancelWaitableTimer(*state.timer) };
        state.waker = None;
        state.fired_counter = None;
        Ok(())
    }
}

impl Future for Timer {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.state.lock();
        state.waker = Some(cx.waker().clone());
        if state.fired_counter != self.last_fired {
            let last_fired = state.fired_counter;
            drop(state);
            self.as_mut().last_fired = last_fired;
            return Poll::Ready(());
        }
        Poll::Pending
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        let mut state = self.state.lock();
        // TODO: log these failures?
        let _ = unsafe { CancelWaitableTimer(*state.timer) };
        if let Some(wait_completion_packet) = state.wait_completion_packet.take() {
            let _ = unsafe { NtCancelWaitCompletionPacket(*wait_completion_packet, 1) }.ok();
        }
        // Tell the background thread to free lpOverlapped.
        let _ = unsafe {
            PostQueuedCompletionStatus(
                *BackgroundTimerThread::get().completion_port().0,
                0,
                BackgroundTimerThread::DROP_KEY,
                Some(self.weak_state_ptr as *const OVERLAPPED),
            )
        };
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use crate::windows::Timer;

    #[tokio::test]
    async fn sleep() {
        let duration = Duration::from_millis(10);
        let now = Instant::now();
        let mut timer = Timer::new().unwrap();
        timer.sleep(duration).unwrap();
        timer.await;
        assert!(now.elapsed() > duration);
    }

    #[tokio::test]
    async fn reset() {
        let duration = Duration::from_millis(1);
        let now = Instant::now();
        let mut timer = Timer::new().unwrap();
        timer.sleep(duration).unwrap();
        timer.clear().unwrap();
        timer.sleep(10 * duration).unwrap();
        timer.await;
        assert!(now.elapsed() > 10 * duration);
    }

    #[tokio::test]
    async fn interval() {
        let duration = Duration::from_millis(10);
        let mut timer = Timer::new().unwrap();
        for _ in 0..5 {
            let now = Instant::now();
            timer.interval(duration).unwrap();
            (&mut timer).await;
            assert!(now.elapsed() > duration);
        }
    }
}

#[cfg(feature = "tokio")]
mod tokio {
    use std::future::Future;
    use std::io::Result;
    use std::pin::Pin;
    use std::time::Duration;

    use crate::shared::tokio::ClockType;
    use crate::windows::Timer;

    impl ClockType {
        /// Equivalent to [`tokio::time::sleep`].
        pub fn sleep(&self, duration: Duration) -> Result<Sleep> {
            let mut timer = Timer::new()?;
            timer.sleep(duration)?;
            Ok(Sleep(timer))
        }

        /// Similar to [`tokio::time::interval`], but with [`tokio::time::MissedTickBehavior::Skip`] as the default tick behavior.
        pub fn interval(&self, duration: Duration) -> Result<Interval> {
            let mut timer = Timer::new()?;
            timer.interval(duration)?;
            Ok(Interval {
                inner: timer,
                duration,
            })
        }

        /// Similar to [`tokio::time::interval_at`], but with [`tokio::time::MissedTickBehavior::Skip`] as the tick behavior.
        pub fn interval_at(
            &self,
            delay_until_start: Duration,
            duration: Duration,
        ) -> Result<Interval> {
            let mut timer = Timer::new()?;
            timer.interval_at(delay_until_start, duration)?;
            Ok(Interval {
                inner: timer,
                duration,
            })
        }
    }

    #[pin_project::pin_project]
    pub struct Sleep(#[pin] Timer);

    impl Sleep {
        fn clear(&mut self) -> Result<()> {
            self.0.clear()
        }

        /// Resets this instance to end after `duration`.
        ///
        /// Calling this instead of recreating the sleep lets you reuse underlying resources.
        pub fn reset(mut self: Pin<&mut Self>, duration: Duration) -> Result<()> {
            self.as_mut().clear()?;
            self.as_mut().0.sleep(duration)
        }
    }

    impl Future for Sleep {
        type Output = Result<()>;

        fn poll(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Self::Output> {
            self.project().0.poll(cx).map(Ok)
        }
    }

    #[pin_project::pin_project]
    pub struct Interval {
        #[pin]
        inner: Timer,
        duration: Duration,
    }

    impl Interval {
        /// Future that waits until the next interval completes.
        pub async fn tick(&mut self) -> Result<()> {
            (&mut self.inner).await;
            Ok(())
        }

        fn clear(&mut self) -> Result<()> {
            self.inner.clear()
        }

        /// Resets this instance to end after the duration specified when it was created.
        ///
        /// Calling this instead of recreating the interval lets you reuse underlying resources.
        pub fn reset(&mut self) -> Result<()> {
            self.clear()?;
            self.inner.interval(self.duration)
        }

        /// Like [`Interval::reset`], but lets you specify a duration after which the interval timer should begin.
        pub fn reset_after(&mut self, after: Duration) -> Result<()> {
            self.clear()?;
            self.inner.interval_at(after, self.duration)
        }
    }
}

#[cfg(feature = "smol")]
mod smol {
    use futures_lite::Stream;

    use std::future::Future;
    use std::io::Result;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::time::Duration;

    use super::Timer as IocpTimer;
    use crate::shared::smol::TimerType;

    impl TimerType {
        /// Equivalent to [`async_io::Timer::after`].
        pub fn after(&self, duration: Duration) -> Result<Timer> {
            let mut timer = IocpTimer::new()?;
            timer.sleep(duration)?;
            Ok(Timer(timer))
        }

        /// Equivalent to [`async_io::Timer::interval`].
        pub fn interval(&self, duration: Duration) -> Result<Timer> {
            let mut timer = IocpTimer::new()?;
            timer.interval(duration)?;
            Ok(Timer(timer))
        }

        /// Equivalent to [`async_io::Timer::interval_at`].
        pub fn interval_at(
            &self,
            delay_until_start: Duration,
            duration: Duration,
        ) -> Result<Timer> {
            let mut timer = IocpTimer::new()?;
            timer.interval_at(delay_until_start, duration)?;
            Ok(Timer(timer))
        }

        /// Equivalent to [`async_io::Timer::never`].
        pub fn never(&self) -> Result<Timer> {
            IocpTimer::new().map(Timer)
        }
    }

    #[pin_project::pin_project]
    pub struct Timer(#[pin] IocpTimer);

    impl Timer {
        /// Resets the timer to never trigger and removes any pending wakeups.
        pub fn clear(&mut self) -> Result<()> {
            self.0.clear()
        }

        /// Equivalent to [`TimerType::after`], but reuses resources.
        ///
        /// Any pending wakeups will be cleared.
        pub fn set_after(&mut self, duration: Duration) -> Result<()> {
            self.clear()?;
            self.0.sleep(duration)
        }

        /// Equivalent to [`TimerType::interval`], but reuses resources.
        ///
        /// Any pending wakeups will be cleared.
        pub fn set_interval(&mut self, period: Duration) -> Result<()> {
            self.clear()?;
            self.0.interval(period)
        }

        /// Equivalent to [`TimerType::interval_at`], but reuses resources.
        ///
        /// Any pending wakeups will be cleared.
        pub fn set_interval_at(&mut self, after: Duration, period: Duration) -> Result<()> {
            self.clear()?;
            self.0.interval_at(after, period)
        }
    }

    impl Stream for Timer {
        type Item = Result<()>;

        fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            // Mirrors the behavior of `async_io::Timer` which doesn't return `None` even if this is a never.
            self.poll(cx).map(Some)
        }
    }

    impl Future for Timer {
        type Output = Result<()>;

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            self.project().0.poll(cx).map(Ok)
        }
    }
}
