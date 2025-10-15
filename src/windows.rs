use std::time::Duration;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, SetWaitableTimer,
    TIMER_MODIFY_STATE,
};

compile_error!("Windows is not supported yet.");

fn new_timer() -> windows::core::Result<HANDLE> {
    unsafe {
        CreateWaitableTimerExW(
            None,
            None,
            CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
            TIMER_MODIFY_STATE.0,
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
