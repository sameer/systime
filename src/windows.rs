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

/// SAFETY: Completion port can safely be concurrently accessed.
#[derive(Clone)]
struct CompletionPort(Arc<Owned<HANDLE>>);

unsafe impl Send for CompletionPort {}
unsafe impl Sync for CompletionPort {}

/// Internal state of the background thread responsible for notifying when timers have fired.
struct BackgroundTimerThread {
    completion_port: CompletionPort,
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
            let completion_port = CompletionPort(Arc::new(unsafe {
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
    fn run(completion_port: CompletionPort) {
        let mut entries = [OVERLAPPED_ENTRY::default(); Self::OVERLAPPED_ENTRY_BUFFER_SIZE];

        loop {
            let mut num_entries = 0;
            // Block indefinitely waiting for timer completions
            let result = unsafe {
                GetQueuedCompletionStatusEx(
                    **completion_port.0,
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
                                **completion_port.0,
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
            **completion_port.0,
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

    /// Cancels any outstanding triggers.
    fn reset(&mut self) -> Result<()> {
        let mut state = self.state.lock();

        // Best-effort stop the timer from firing. Even if it does fire,
        // we shouldn't get a wakeup because the waker was cleared.
        //
        // TODO: we should likely store a generation counter to skip wakeups from before a reset.
        let _ = unsafe { CancelWaitableTimer(*state.timer) };
        if let Some(wait_completion_packet) = state.wait_completion_packet.as_ref() {
            let _ = unsafe { NtCancelWaitCompletionPacket(**wait_completion_packet, 1) }.ok()?;
        }

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
            let last_fired = state.fired_counter.clone();
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
                **BackgroundTimerThread::get().completion_port().0,
                0,
                BackgroundTimerThread::DROP_KEY,
                Some(self.weak_state_ptr as *const OVERLAPPED),
            )
        };
    }
}
