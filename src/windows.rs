use std::os::raw::c_void;
use std::ptr;
use std::time::Duration;

use windows::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows::Win32::Foundation::{
    ERROR_TIMEOUT, GENERIC_ACCESS_RIGHTS, GENERIC_ALL, HANDLE, INVALID_HANDLE_VALUE, NTSTATUS,
};
use windows::Win32::System::IO::{
    CreateIoCompletionPort, GetQueuedCompletionStatusEx, OVERLAPPED_ENTRY,
};
use windows::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, SYNCHRONIZATION_SYNCHRONIZE,
    SetWaitableTimer, TIMER_MODIFY_STATE, TIMER_QUERY_STATE,
};

fn new_completion_port() -> windows::core::Result<HANDLE> {
    /// This is not returned anywhere
    const COMPLETION_KEY: usize = 0;
    /// Completion status will only be checked on a single thread`.
    const CONCURRENCY: u32 = 1;
    unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, None, COMPLETION_KEY, CONCURRENCY) }
}

fn new_wait_completion_packet() -> windows::core::Result<HANDLE> {
    #[link(name = "ntdll")]
    unsafe extern "system" {
        unsafe fn NtCreateWaitCompletionPacket(
            WaitCompletionPacketHandle: *mut HANDLE,
            DesiredAccess: GENERIC_ACCESS_RIGHTS,
            ObjectAttributes: *const OBJECT_ATTRIBUTES,
        ) -> NTSTATUS;
    }

    let mut handle = HANDLE::default();
    unsafe { NtCreateWaitCompletionPacket(&mut handle, GENERIC_ALL, ptr::null()) }.ok()?;
    Ok(handle)
}

fn enqueue_timer(timer: &HANDLE) -> windows::core::Result<(HANDLE, bool)> {
    #[link(name = "ntdll")]
    unsafe extern "system" {
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
    }

    let wait_completion_packet = new_wait_completion_packet()?;
    let completion_port = new_completion_port()?;
    let key_ctx = ptr::null();
    let apc_ctx = ptr::null();
    let mut signaled = 0u8;
    unsafe {
        NtAssociateWaitCompletionPacket(
            wait_completion_packet,
            completion_port,
            *timer,
            key_ctx,
            apc_ctx,
            NTSTATUS::default(),
            0,
            &mut signaled,
        )
    }
    .ok()
    .unwrap();

    Ok((completion_port, signaled != 0))
}

fn poll_expiry(completion_port: &HANDLE) -> windows::core::Result<bool> {
    let mut entries = [OVERLAPPED_ENTRY::default(); 1];
    let mut len = 0;
    let res =
        unsafe { GetQueuedCompletionStatusEx(*completion_port, &mut entries, &mut len, 0, false) };

    if res.is_err_and(|err| err.code() == ERROR_TIMEOUT.to_hresult()) {
        Ok(false)
    } else {
        Ok(len > 0)
    }
}

fn new_timer() -> windows::core::Result<HANDLE> {
    unsafe {
        CreateWaitableTimerExW(
            None,
            None,
            // TODO: check for high resoultion timer support on this system
            CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
            TIMER_MODIFY_STATE.0 | TIMER_QUERY_STATE.0 | SYNCHRONIZATION_SYNCHRONIZE.0,
        )
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
