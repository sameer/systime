//! Timer implementation using [kqueue](https://en.wikipedia.org/wiki/Kqueue) on Apple targets (macOS/iOS/tvOS/watchOS/visionOS).
//!
//! Each timer is registered with its own kqueue. A kevent is received when the timer fires,
//! waking the task.

use std::io::{ErrorKind, Result};
use std::{io::Error, time::Duration};

use nix::fcntl::FdFlag;
use nix::sys::event::EventFilter;
use nix::{
    fcntl::{FcntlArg, fcntl},
    sys::{
        event::{EvFlags, FilterFlag, KEvent, Kqueue},
        time::TimeSpec,
    },
};

const KQUEUE_EVFILT_TIMER_ID: usize = 0x54494D45;
const KQUEUE_EVENT_TIMEOUT: TimeSpec = TimeSpec::from_duration(Duration::ZERO);

const SLEEP_TIMER_FLAGS: EvFlags = EvFlags::from_bits(
    EvFlags::EV_ADD.bits() | EvFlags::EV_ENABLE.bits() | EvFlags::EV_ONESHOT.bits(),
)
.unwrap();
const INTERVAL_TIMER_FLAGS: EvFlags =
    EvFlags::from_bits(EvFlags::EV_ADD.bits() | EvFlags::EV_ENABLE.bits()).unwrap();

const CONTINUOUS_TIME_FILTER_FLAGS: FilterFlag =
    FilterFlag::from_bits_retain(libc::NOTE_NSECONDS | libc::NOTE_MACH_CONTINUOUS_TIME);
const ABSOLUTE_TIME_FILTER_FLAGS: FilterFlag = FilterFlag::from_bits_retain(libc::NOTE_NSECONDS);

/// Creates a [`Kqueue`] with [`FdFlag::FD_CLOEXEC`] set.
fn kqueue() -> Result<Kqueue> {
    let kqueue = Kqueue::new()?;
    fcntl(&kqueue, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;
    Ok(kqueue)
}

/// Adds a new timer to the [`Kqueue`] with the provided parameters.
///
/// Any existing timer with identifier [`KQUEUE_EVFILT_TIMER_ID`] will be replaced.
///
/// <https://keith.github.io/xcode-man-pages/kqueue.2.html>
fn add_timer(
    kqueue: &Kqueue,
    include_system_sleep: bool,
    is_oneshot: bool,
    duration: Duration,
) -> Result<()> {
    let duration = duration.as_nanos().try_into().map_err(|_err| {
        Error::new(
            ErrorKind::InvalidInput,
            "Duration cannot be represented in isize nanoseconds",
        )
    })?;
    let changelist = [KEvent::new(
        KQUEUE_EVFILT_TIMER_ID,
        EventFilter::EVFILT_TIMER,
        if is_oneshot {
            SLEEP_TIMER_FLAGS
        } else {
            INTERVAL_TIMER_FLAGS
        },
        if include_system_sleep {
            CONTINUOUS_TIME_FILTER_FLAGS
        } else {
            ABSOLUTE_TIME_FILTER_FLAGS
        },
        duration,
        0,
    )];
    kqueue.kevent(
        changelist.as_slice(),
        &mut [],
        Some(*KQUEUE_EVENT_TIMEOUT.as_ref()),
    )?;

    Ok(())
}

/// Removes a timer added by [`add_timer`] from the [`Kqueue`].
fn remove_timer(kqueue: &Kqueue) -> Result<()> {
    let changelist = [KEvent::new(
        KQUEUE_EVFILT_TIMER_ID,
        EventFilter::EVFILT_TIMER,
        EvFlags::EV_DELETE,
        FilterFlag::empty(),
        0,
        0,
    )];

    let mut event_list = [KEvent::new(
        0,
        nix::sys::event::EventFilter::EVFILT_TIMER,
        EvFlags::empty(),
        FilterFlag::empty(),
        0,
        0,
    ); 1];
    kqueue
        .kevent(
            changelist.as_slice(),
            &mut event_list,
            Some(*KQUEUE_EVENT_TIMEOUT.as_ref()),
        )
        .map(|_| ())?;
    Ok(())
}

#[cfg(feature = "tokio")]
mod tokio {
    use std::future::Future;
    use std::io::Result;
    use std::os::fd::{AsFd, AsRawFd, RawFd};
    use std::pin::Pin;
    use std::task::ready;
    use std::time::Duration;

    use nix::sys::event::{EvFlags, FilterFlag, KEvent, Kqueue};
    use tokio::io::Interest;
    use tokio::io::unix::AsyncFd;

    use crate::apple::KQUEUE_EVENT_TIMEOUT;
    use crate::shared::tokio::ClockType;

    impl ClockType {
        const fn include_system_sleep(self) -> bool {
            match self {
                ClockType::IgnoreSleep => false,
                ClockType::TrackSleep => true,
            }
        }

        /// Equivalent to [`tokio::time::sleep`].
        pub fn sleep(&self, duration: Duration) -> Result<Sleep> {
            let kqueue = super::kqueue()?;
            super::add_timer(&kqueue, self.include_system_sleep(), true, duration)?;
            Ok(Sleep {
                fd: TokioKqueueFd::new(kqueue)?,
                ty: *self,
            })
        }

        /// Similar to [`tokio::time::interval`], but with [`tokio::time::MissedTickBehavior::Skip`] as the default tick behavior.
        pub fn interval(&self, duration: Duration) -> Result<Interval> {
            let kqueue = super::kqueue()?;
            super::add_timer(&kqueue, self.include_system_sleep(), false, duration)?;
            Ok(Interval {
                fd: TokioKqueueFd::new(kqueue)?,
                ty: *self,
                duration,
            })
        }
    }

    #[pin_project::pin_project]
    pub struct Sleep {
        #[pin]
        fd: TokioKqueueFd,
        ty: ClockType,
    }

    impl Sleep {
        /// Resets this instance to end after `duration`.
        ///
        /// Calling this instead of recreating the sleep lets you reuse underlying resources.
        pub fn reset(self: Pin<&mut Self>, duration: Duration) -> Result<()> {
            super::remove_timer(&self.fd.0.get_ref().0)?;
            super::add_timer(
                &self.fd.0.get_ref().0,
                self.ty.include_system_sleep(),
                true,
                duration,
            )
        }
    }

    impl Future for Sleep {
        type Output = Result<()>;

        fn poll(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Self::Output> {
            self.project().fd.poll(cx)
        }
    }

    #[pin_project::pin_project]
    pub struct Interval {
        #[pin]
        fd: TokioKqueueFd,
        ty: ClockType,
        duration: Duration,
    }

    impl Interval {
        /// Future that waits until the next interval completes.
        pub async fn tick(&mut self) -> Result<()> {
            (&mut self.fd).await
        }

        /// Resets this instance to end after the duration specified when it was created.
        ///
        /// Calling this instead of recreating the interval lets you reuse underlying resources.
        pub fn reset(&mut self) -> Result<()> {
            super::remove_timer(&self.fd.0.get_ref().0)?;
            super::add_timer(
                &self.fd.0.get_ref().0,
                self.ty.include_system_sleep(),
                false,
                self.duration,
            )
        }
    }

    struct TokioKqueueFd(AsyncFd<WrappedKqueue>);

    /// Required because [`Kqueue`] does not implement [`AsRawFd`] needed by [`AsyncFd`].
    struct WrappedKqueue(Kqueue);

    impl AsRawFd for WrappedKqueue {
        fn as_raw_fd(&self) -> RawFd {
            self.0.as_fd().as_raw_fd()
        }
    }

    impl TokioKqueueFd {
        fn new(kqueue: Kqueue) -> Result<Self> {
            AsyncFd::with_interest(WrappedKqueue(kqueue), Interest::READABLE).map(Self)
        }
    }

    impl Future for TokioKqueueFd {
        type Output = Result<()>;

        fn poll(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Self::Output> {
            let mut event_list = [KEvent::new(
                0,
                nix::sys::event::EventFilter::EVFILT_TIMER,
                EvFlags::empty(),
                FilterFlag::empty(),
                0,
                0,
            ); 1];

            loop {
                let mut guard = ready!(self.0.poll_read_ready(cx))?;
                match guard.try_io(|inner| {
                    let num_events = inner.get_ref().0.kevent(
                        &[],
                        &mut event_list,
                        Some(*KQUEUE_EVENT_TIMEOUT.as_ref()),
                    )?;

                    // kqueue is technically a blocking API, but polling
                    // it with a zero timeout makes it nonblocking-ish?
                    if num_events > 0 {
                        let [event] = event_list;
                        Ok(event)
                    } else {
                        Err(std::io::Error::new(
                            std::io::ErrorKind::WouldBlock,
                            "kevent call timed out and returned nothing",
                        ))
                    }
                }) {
                    Ok(Ok(_event)) => break std::task::Poll::Ready(Ok(())),
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
    use nix::sys::event::{EvFlags, EventFilter, FilterFlag, KEvent, Kqueue};

    use std::future::Future;
    use std::io::{Error, Result};
    use std::pin::Pin;
    use std::task::{Context, Poll, ready};
    use std::time::Duration;

    use crate::apple::KQUEUE_EVENT_TIMEOUT;
    use crate::shared::smol::TimerType;

    impl TimerType {
        const fn include_system_sleep(self) -> bool {
            match self {
                TimerType::IgnoreSleep => false,
                TimerType::TrackSleep => true,
            }
        }

        /// Equivalent to [`async_io::Timer::after`].
        pub fn after(&self, duration: Duration) -> Result<Timer> {
            let kqueue = super::kqueue()?;
            super::add_timer(&kqueue, self.include_system_sleep(), true, duration)?;
            Ok(Timer {
                fd: SmolKqueueFd::new(kqueue)?,
                ty: *self,
            })
        }

        /// Equivalent to [`async_io::Timer::interval`].
        pub fn interval(&self, duration: Duration) -> Result<Timer> {
            let kqueue = super::kqueue()?;
            super::add_timer(&kqueue, self.include_system_sleep(), false, duration)?;
            Ok(Timer {
                fd: SmolKqueueFd::new(kqueue)?,
                ty: *self,
            })
        }

        /// Equivalent to [`async_io::Timer::never`].
        pub fn never(&self) -> Result<Timer> {
            super::kqueue()
                .and_then(SmolKqueueFd::new)
                .map(|fd| Timer { fd, ty: *self })
        }
    }

    #[pin_project::pin_project]
    pub struct Timer {
        #[pin]
        fd: SmolKqueueFd,
        ty: TimerType,
    }

    impl Timer {
        /// Resets the timer to never trigger and removes any pending wakeups.
        pub fn clear(&mut self) -> Result<()> {
            super::remove_timer(&self.fd.0.get_ref())
        }

        /// Equivalent to [`TimerType::after`], but reuses resources.
        pub fn set_after(&mut self, duration: Duration) -> Result<()> {
            self.clear()?;
            super::add_timer(
                self.fd.0.get_ref(),
                self.ty.include_system_sleep(),
                true,
                duration,
            )
            .map_err(Error::from)
        }

        /// Equivalent to [`TimerType::interval`], but reuses resources.
        pub fn set_interval(&mut self, period: Duration) -> Result<()> {
            self.clear()?;
            super::add_timer(
                self.fd.0.get_ref(),
                self.ty.include_system_sleep(),
                false,
                period,
            )
            .map_err(Error::from)
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
            self.project().fd.poll(cx)
        }
    }

    struct SmolKqueueFd(Async<Kqueue>);

    impl SmolKqueueFd {
        pub fn new(kqueue: Kqueue) -> Result<Self> {
            // `kqueue` is blocking but we call it in a non-blocking way
            Async::new_nonblocking(kqueue).map(Self)
        }
    }

    impl Future for SmolKqueueFd {
        type Output = Result<()>;

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let mut event_list = [KEvent::new(
                0,
                EventFilter::EVFILT_TIMER,
                EvFlags::empty(),
                FilterFlag::empty(),
                0,
                0,
            ); 1];

            // Follows the pattern seen in the `AsyncRead` impl for `Async`.
            loop {
                let num_events = self.0.get_ref().kevent(
                    &[],
                    &mut event_list,
                    Some(*KQUEUE_EVENT_TIMEOUT.as_ref()),
                )?;

                if num_events > 0 {
                    let [_event] = event_list;
                    break Poll::Ready(Ok(()));
                }

                ready!(self.0.poll_readable(cx))?;
            }
        }
    }
}
