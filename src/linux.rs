use std::time::Duration;

use nix::sys::{
    time::TimeSpec,
    timerfd::{ClockId, Expiration, TimerFd, TimerFlags, TimerSetTimeFlags},
};

fn new_timer(clock_id: ClockId) -> Result<TimerFd, std::io::Error> {
    TimerFd::new(clock_id, TimerFlags::TFD_NONBLOCK | TimerFlags::TFD_CLOEXEC)
        .map_err(std::io::Error::from)
}

fn sleep(timer_fd: &TimerFd, duration: Duration) -> Result<(), std::io::Error> {
    timer_fd
        .set(
            Expiration::OneShot(TimeSpec::from_duration(duration)),
            TimerSetTimeFlags::empty(),
        )
        .map_err(std::io::Error::from)
}

fn interval(timer_fd: &TimerFd, duration: Duration) -> Result<(), std::io::Error> {
    timer_fd
        .set(
            Expiration::Interval(TimeSpec::from_duration(duration)),
            TimerSetTimeFlags::empty(),
        )
        .map_err(std::io::Error::from)
}

fn interval_at(
    timer_fd: &TimerFd,
    delay_until_start: Duration,
    duration: Duration,
) -> Result<(), std::io::Error> {
    timer_fd
        .set(
            Expiration::IntervalDelayed(
                TimeSpec::from_duration(delay_until_start),
                TimeSpec::from_duration(duration),
            ),
            TimerSetTimeFlags::empty(),
        )
        .map_err(std::io::Error::from)
}

#[cfg(feature = "tokio")]
mod tokio {
    use std::future::Future;
    use std::io::{Error, ErrorKind, Result};
    use std::os::fd::{AsFd, AsRawFd, RawFd};
    use std::pin::Pin;
    use std::task::ready;
    use std::time::Duration;

    use nix::sys::timerfd::{ClockId, TimerFd};
    use tokio::io::Interest;
    use tokio::io::unix::AsyncFd;

    use crate::shared::tokio::ClockType;

    impl ClockType {
        const fn clock_id(self) -> ClockId {
            match self {
                ClockType::IgnoreSleep => ClockId::CLOCK_MONOTONIC,
                ClockType::TrackSleep => ClockId::CLOCK_BOOTTIME,
            }
        }

        /// Equivalent to [`tokio::time::sleep`].
        pub fn sleep(&self, duration: Duration) -> Result<Sleep> {
            super::new_timer(self.clock_id())
                .and_then(|timer_fd| {
                    super::sleep(&timer_fd, duration)?;
                    Ok(timer_fd)
                })
                .and_then(TokioTimerFd::new)
                .map(Sleep)
        }

        /// Similar to [`tokio::time::interval`], but with [`tokio::time::MissedTickBehavior::Skip`] as the default tick behavior.
        pub fn interval(&self, duration: Duration) -> Result<Interval> {
            super::new_timer(self.clock_id())
                .and_then(|timer_fd| {
                    super::interval(&timer_fd, duration)?;
                    Ok(timer_fd)
                })
                .and_then(TokioTimerFd::new)
                .map(|fd| Interval { fd, duration })
        }

        /// Similar to [`tokio::time::interval_at`], but with [`tokio::time::MissedTickBehavior::Skip`] as the tick behavior.
        pub fn interval_at(
            &self,
            delay_until_start: Duration,
            duration: Duration,
        ) -> Result<Interval> {
            super::new_timer(self.clock_id())
                .and_then(|timer_fd| {
                    super::interval_at(&timer_fd, delay_until_start, duration)?;
                    Ok(timer_fd)
                })
                .and_then(TokioTimerFd::new)
                .map(|fd| Interval { fd, duration })
        }
    }

    #[pin_project::pin_project]
    pub struct Sleep(#[pin] TokioTimerFd);

    impl Sleep {
        fn clear(&mut self) -> Result<()> {
            self.0.0.get_ref().0.unset()?;
            match self.0.0.get_ref().0.wait().map_err(Error::from) {
                Ok(()) => Ok(()),
                Err(err) if err.kind() == ErrorKind::WouldBlock => Ok(()),
                Err(other) => Err(other),
            }
        }

        /// Resets this instance to end after `duration`.
        ///
        /// Calling this instead of recreating the sleep lets you reuse underlying resources.
        pub fn reset(mut self: Pin<&mut Self>, duration: Duration) -> Result<()> {
            self.as_mut().clear()?;
            super::sleep(&self.0.0.get_ref().0, duration)
        }
    }

    impl Future for Sleep {
        type Output = Result<()>;

        fn poll(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Self::Output> {
            self.project().0.poll(cx)
        }
    }

    #[pin_project::pin_project]
    pub struct Interval {
        #[pin]
        fd: TokioTimerFd,
        duration: Duration,
    }

    impl Interval {
        /// Future that waits until the next interval completes.
        pub async fn tick(&mut self) -> Result<()> {
            (&mut self.fd).await
        }

        fn clear(&mut self) -> Result<()> {
            self.fd.0.get_ref().0.unset()?;
            match self.fd.0.get_ref().0.wait().map_err(Error::from) {
                Ok(()) => Ok(()),
                Err(err) if err.kind() == ErrorKind::WouldBlock => Ok(()),
                Err(other) => Err(other),
            }
        }

        /// Resets this instance to end after the duration specified when it was created.
        ///
        /// Calling this instead of recreating the interval lets you reuse underlying resources.
        pub fn reset(&mut self) -> Result<()> {
            self.clear()?;
            super::interval(&self.fd.0.get_ref().0, self.duration)
        }

        /// Like [`Interval::reset`], but lets you specify a duration after which the interval timer should begin.
        pub fn reset_after(&mut self, after: Duration) -> Result<()> {
            self.clear()?;
            super::interval_at(&self.fd.0.get_ref().0, after, self.duration)
        }
    }

    struct TokioTimerFd(AsyncFd<WrappedTimerFd>);

    /// Required because [`TimerFd`] does not implement [`AsRawFd`] needed by [`AsyncFd`].
    struct WrappedTimerFd(TimerFd);

    impl AsRawFd for WrappedTimerFd {
        fn as_raw_fd(&self) -> RawFd {
            self.0.as_fd().as_raw_fd()
        }
    }

    impl TokioTimerFd {
        fn new(timerfd: TimerFd) -> Result<Self> {
            AsyncFd::with_interest(WrappedTimerFd(timerfd), Interest::READABLE).map(Self)
        }
    }

    impl Future for TokioTimerFd {
        type Output = Result<()>;

        fn poll(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Self::Output> {
            loop {
                let mut guard = ready!(self.0.poll_read_ready(cx))?;

                match guard.try_io(|inner| inner.get_ref().0.wait().map_err(Error::from)) {
                    Ok(Ok(())) => break std::task::Poll::Ready(Ok(())),
                    Ok(Err(e)) => break std::task::Poll::Ready(Err(e)),
                    Err(_would_block) => continue,
                }
            }
        }
    }
}

#[cfg(feature = "smol")]
mod smol {
    use async_io::Async;
    use futures_lite::Stream;
    use nix::sys::timerfd::{ClockId, TimerFd};

    use std::future::Future;
    use std::io::{Error, ErrorKind, Result};
    use std::pin::Pin;
    use std::task::{Context, Poll, ready};
    use std::time::Duration;

    use crate::shared::smol::TimerType;

    impl TimerType {
        const fn clock_id(self) -> ClockId {
            match self {
                TimerType::IgnoreSleep => ClockId::CLOCK_MONOTONIC,
                TimerType::TrackSleep => ClockId::CLOCK_BOOTTIME,
            }
        }

        /// Equivalent to [`async_io::Timer::after`].
        pub fn after(&self, duration: Duration) -> Result<Timer> {
            super::new_timer(self.clock_id())
                .and_then(|timer_fd| {
                    super::sleep(&timer_fd, duration)?;
                    Ok(timer_fd)
                })
                .and_then(SmolTimerFd::new)
                .map(Timer)
        }

        /// Equivalent to [`async_io::Timer::interval`].
        pub fn interval(&self, duration: Duration) -> Result<Timer> {
            super::new_timer(self.clock_id())
                .and_then(|timer_fd| {
                    super::interval(&timer_fd, duration)?;
                    Ok(timer_fd)
                })
                .and_then(SmolTimerFd::new)
                .map(Timer)
        }

        /// Equivalent to [`async_io::Timer::interval_at`].
        pub fn interval_at(
            &self,
            delay_until_start: Duration,
            duration: Duration,
        ) -> Result<Timer> {
            super::new_timer(self.clock_id())
                .and_then(|timer_fd| {
                    super::interval_at(&timer_fd, delay_until_start, duration)?;
                    Ok(timer_fd)
                })
                .and_then(SmolTimerFd::new)
                .map(Timer)
        }

        /// Equivalent to [`async_io::Timer::never`].
        pub fn never(&self) -> Result<Timer> {
            super::new_timer(self.clock_id())
                .and_then(SmolTimerFd::new)
                .map(Timer)
        }
    }

    #[pin_project::pin_project]
    pub struct Timer(#[pin] SmolTimerFd);

    impl Timer {
        /// Resets the timer to never trigger and removes any pending wakeups.
        pub fn clear(&mut self) -> Result<()> {
            self.0.0.get_ref().unset()?;
            match self.0.0.get_ref().wait().map_err(Error::from) {
                Ok(()) => Ok(()),
                Err(err) if err.kind() == ErrorKind::WouldBlock => Ok(()),
                Err(other) => Err(other),
            }
        }

        /// Equivalent to [`TimerType::after`], but reuses resources.
        ///
        /// Any pending wakeups will be cleared.
        pub fn set_after(&mut self, duration: Duration) -> Result<()> {
            self.clear()?;
            super::sleep(self.0.0.get_ref(), duration)
        }

        /// Equivalent to [`TimerType::interval`], but reuses resources.
        ///
        /// Any pending wakeups will be cleared.
        pub fn set_interval(&mut self, period: Duration) -> Result<()> {
            self.clear()?;
            super::interval(self.0.0.get_ref(), period)
        }

        /// Equivalent to [`TimerType::interval_at`], but reuses resources.
        ///
        /// Any pending wakeups will be cleared.
        pub fn set_interval_at(&mut self, after: Duration, period: Duration) -> Result<()> {
            self.clear()?;
            super::interval_at(self.0.0.get_ref(), after, period)
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
            self.project().0.poll(cx)
        }
    }

    struct SmolTimerFd(Async<TimerFd>);

    impl SmolTimerFd {
        pub fn new(timerfd: TimerFd) -> Result<Self> {
            // `timerfd` is already non-blocking
            Async::new_nonblocking(timerfd).map(Self)
        }
    }

    impl Future for SmolTimerFd {
        type Output = Result<()>;

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            // Follows the pattern seen in the `AsyncRead` impl for `Async`.
            loop {
                match self.as_ref().0.get_ref().wait().map_err(Error::from) {
                    Ok(()) => break Poll::Ready(Ok(())),
                    // Spurious wakeup
                    Err(err) if err.kind() == ErrorKind::WouldBlock => {}
                    Err(other) => {
                        break Poll::Ready(Err(other));
                    }
                }
                ready!(self.0.poll_readable(cx))?;
            }
        }
    }
}
