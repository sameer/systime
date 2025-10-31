//! Portable async timers that handle system sleep consistently across platforms.
//!
//! # Motivation
//!
//! Timer implementations in std, tokio, and other crates behave differently when the system sleeps:
//!
//! - **Linux/Android**: `CLOCK_MONOTONIC` excludes time spent sleeping
//! - **macOS/iOS**: `CLOCK_UPTIME_RAW` (or `mach_absolute_time`) excludes time spent sleeping
//! - **Windows**: `QueryPerformanceCounter` *includes* time spent sleeping ([rust-lang/rust#79462](https://github.com/rust-lang/rust/issues/79462))
//!
//! This makes it difficult to write portable code with predictable timer behavior,
//! especially in applications that need to account for the real-world time that has passed (including system sleep).
//!
//! `systime` offers a simple API where you explicitly choose whether to track or ignore system sleep.
//!
//! # Platform Support
//!
//! - **Linux/Android**: `timerfd` with `CLOCK_MONOTONIC` (ignore sleep) or `CLOCK_BOOTTIME` (track sleep)
//! - **macOS/iOS**: `kqueue` + `EVFILT_TIMER` with mach absolute time (ignore sleep) or mach continuous time (track sleep)
//! - **Windows**: partial -- sleep is always tracked
//!
//! # Runtime Support
//!
//! `systime` supports both the `tokio` and `smol` async ecosystems via feature flags.
//! Tokio support is enabled by default.
//!
//! # Use Cases
//!
//! Tracking sleep is useful for:
//!
//! - **Networking timers**: Any timeouts, keepalives, and other networking timers likely need to be aware of system sleep.
//! - **Authentication**: Credentials may expire sooner than expected unless system sleep is tracked.
//! - **User-facing scheduled tasks**: Run tasks at specific intervals regardless of system sleep (i.e. UI reminder popup).
//! - **Consistent cross-device timing**: When correlating events between devices, it's important to know the real world time that has passed.
//!
//! Ignoring sleep is useful for:
//!
//! - **Internal scheduled tasks**: Run tasks at specific intervals in accordance with time spent executing (i.e. garbage collection).
//! - **Performance monitoring / profiling**: Benchmarks should not track system sleep, because only the execution time matters.
//!
//! # Performance
//!
//! This library uses system OS timer facilities. There are limits on these resources, hence why each operation can return an error.
//! Use these timers _sparingly_ and include a fallback to the appropriate equivalent (i.e. tokio::time for tokio) in case of failure.
//!
//! # Examples
//!
#![cfg_attr(feature = "tokio", doc = "## Basic sleep with tokio")]
#![cfg_attr(
    feature = "tokio",
    doc = r#"
```no_run
use std::time::Duration;
use systime::tokio::ClockType;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    // Sleep for 5 seconds, pausing if the system sleeps
    ClockType::IgnoreSleep
        .sleep(Duration::from_secs(5))?
        .await?;

    // Sleep for 5 seconds, including any system sleep time
    ClockType::TrackSleep
        .sleep(Duration::from_secs(5))?
        .await?;

    Ok(())
}
```
"#
)]
//!
#![cfg_attr(feature = "tokio", doc = "## Intervals with tokio")]
#![cfg_attr(
    feature = "tokio",
    doc = r#"
```no_run
use std::time::Duration;
use systime::tokio::ClockType;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let mut interval = ClockType::IgnoreSleep
        .interval(Duration::from_secs(1))?;

    for _ in 0..10 {
        interval.tick().await?;
        println!("Tick!");
    }

    Ok(())
}
```
"#
)]
//!
#![cfg_attr(feature = "smol", doc = "## smol support")]
#![cfg_attr(
    feature = "smol",
    doc = r#"
```no_run
use std::time::Duration;
use systime::smol::TimerType;
use futures_lite::StreamExt;

fn main() -> std::io::Result<()> {
    futures_lite::future::block_on(async {
        // One-shot timer
        TimerType::IgnoreSleep
            .after(Duration::from_secs(1))?
            .await?;

        // Interval as a stream
        let mut interval = TimerType::TrackSleep
            .interval(Duration::from_millis(500))?;

        while let Some(result) = interval.next().await {
            result?;
            println!("Tick!");
        }

        Ok(())
    })
}
```
"#
)]
//!
#![cfg_attr(feature = "tokio", doc = "## Reusable timers")]
#![cfg_attr(
    feature = "tokio",
    doc = "systime supports reuse to avoid allocating new system resources. In the tokio API:"
)]
#![cfg_attr(
    feature = "tokio",
    doc = r#"
```no_run
use std::time::Duration;
use systime::tokio::ClockType;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let mut sleep = Box::pin(
        ClockType::IgnoreSleep.sleep(Duration::from_secs(1))?
    );

    sleep.as_mut().await?;
    println!("First sleep done");

    // Reuse the same timer
    sleep.as_mut().reset(Duration::from_secs(1))?;
    sleep.await?;
    println!("Second sleep done");

    Ok(())
}
```
"#
)]

#[cfg(feature = "smol")]
pub use shared::smol;
#[cfg(feature = "tokio")]
pub use shared::tokio;

#[cfg(target_vendor = "apple")]
mod apple;
#[cfg(any(target_os = "linux", target_os = "android"))]
mod linux;
#[cfg(not(any(
    target_vendor = "apple",
    target_os = "linux",
    target_os = "android",
    target_os = "windows",
)))]
mod unsupported;
#[cfg(target_os = "windows")]
mod windows;

mod shared;
