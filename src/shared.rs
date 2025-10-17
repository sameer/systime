/// Portable timers for use with the tokio ecosystem.
#[cfg(feature = "tokio")]
pub mod tokio {
    #[derive(Default, Clone, Copy)]
    pub enum ClockType {
        /// Increases monotonically, excluding system sleep time.
        #[default]
        IgnoreSleep,
        /// Increases monotonically, including system sleep time.
        TrackSleep,
    }
}

/// Portable timers for use with the smol ecosystem.
#[cfg(feature = "smol")]
pub mod smol {
    #[derive(Default, Clone, Copy)]
    pub enum TimerType {
        /// Increases monotonically, excluding system sleep time.
        #[default]
        IgnoreSleep,
        /// Increases monotonically, including system sleep time.
        TrackSleep,
    }
}

#[cfg(all(test, not(target_os = "windows")))]
mod tests {
    use std::time::{Duration, Instant};

    #[cfg(feature = "smol")]
    use futures_lite::StreamExt;

    #[cfg(feature = "smol")]
    use crate::smol::TimerType;
    #[cfg(feature = "tokio")]
    use crate::tokio::ClockType;

    #[cfg(feature = "tokio")]
    #[tokio::test]
    async fn tokio_sleep() {
        let duration = Duration::from_millis(100);
        for clock_type in [ClockType::TrackSleep, ClockType::IgnoreSleep] {
            let now = Instant::now();
            clock_type
                .sleep(duration)
                .expect("timerfd created successfully")
                .await
                .expect("timerfd awaited successfully");
            let elapsed = now.elapsed();
            assert!(elapsed >= duration);
            assert!(elapsed - duration <= Duration::from_millis(1));
        }
    }

    #[cfg(feature = "tokio")]
    #[tokio::test]
    async fn tokio_sleep_triggers_after_reset() {
        let never = Duration::from_secs(1_000_000);
        let duration = Duration::from_millis(100);
        for clock_type in [ClockType::TrackSleep, ClockType::IgnoreSleep] {
            let now = Instant::now();
            let mut sleep = Box::pin(
                clock_type
                    .sleep(never)
                    .expect("timerfd created successfully"),
            );
            sleep
                .as_mut()
                .reset(duration)
                .expect("timerfd reset successfully");
            sleep.await.expect("timerfd awaited successfully");
            let elapsed = now.elapsed();
            assert!(elapsed >= duration);
            assert!(elapsed - duration <= Duration::from_millis(1));
        }
    }

    #[cfg(feature = "tokio")]
    #[tokio::test]
    async fn tokio_interval() {
        let duration = Duration::from_millis(50);
        for clock_type in [ClockType::TrackSleep, ClockType::IgnoreSleep] {
            let now = Instant::now();
            let mut interval = clock_type
                .interval(duration)
                .expect("timerfd created successfully");

            for i in 1..=2 {
                interval.tick().await.expect("timerfd awaited successfully");
                let elapsed = now.elapsed();
                assert!(elapsed >= duration);
                assert!(elapsed - i * duration <= i * Duration::from_millis(1));
            }
        }
    }

    #[cfg(feature = "tokio")]
    #[tokio::test]
    async fn tokio_interval_triggers_after_reset() {
        let duration = Duration::from_millis(50);
        for clock_type in [ClockType::TrackSleep, ClockType::IgnoreSleep] {
            let now = Instant::now();
            let mut interval = clock_type
                .interval(duration)
                .expect("timerfd created successfully");

            for i in 1..=2 {
                interval.tick().await.expect("timerfd awaited successfully");
                let elapsed = now.elapsed();
                assert!(elapsed >= duration);
                assert!(elapsed - i * duration <= i * Duration::from_millis(1));
            }
        }
    }

    #[cfg(feature = "smol")]
    #[test]
    fn smol_after() {
        let duration = Duration::from_millis(100);
        for timer_type in [TimerType::TrackSleep, TimerType::IgnoreSleep] {
            let now = Instant::now();
            futures_lite::future::block_on(
                timer_type
                    .after(duration)
                    .expect("timerfd created successfully"),
            )
            .expect("timerfd awaited successfully");
            let elapsed = now.elapsed();
            assert!(elapsed >= duration);
            assert!(elapsed - duration <= Duration::from_millis(1));
        }
    }

    #[cfg(feature = "smol")]
    #[test]
    fn smol_interval() {
        let duration = Duration::from_millis(50);
        for timer_type in [TimerType::TrackSleep, TimerType::IgnoreSleep] {
            let now = Instant::now();
            let mut interval = timer_type
                .interval(duration)
                .expect("timerfd created successfully");

            for i in 1..=2 {
                futures_lite::future::block_on(interval.next())
                    .expect("timerfd awaited successfully")
                    .expect("stream never ends");
                let elapsed = now.elapsed();
                assert!(elapsed >= duration);
                assert!(elapsed - i * duration <= i * Duration::from_millis(1));
            }
        }
    }
}
