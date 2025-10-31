#![doc = include_str!("../README.md")]

//! ## Examples
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
