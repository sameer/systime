# systime

 A Rust crate for portable timers that handle system sleep consistently.

## Motivation

Time in the standard library, tokio, and generally every Rust crate has platform-dependent behavior:

- **Linux/Android**: `CLOCK_MONOTONIC` excludes time spent sleeping ([rust-lang/rust#71860](https://github.com/rust-lang/rust/issues/71860))
- **macOS/iOS**: `CLOCK_UPTIME_RAW` (or `mach_absolute_time`) excludes time spent sleeping
- **Windows**: `QueryPerformanceCounter` *includes* time spent sleeping ([rust-lang/rust#79462](https://github.com/rust-lang/rust/issues/79462))

This makes it difficult to write portable code with predictable timer behavior, especially in applications that need to account for the real-world time that has passed (including system sleep).

`systime` offers a simple API where you explicitly choose whether to track or ignore system sleep.

### What is "sleep"?

Sleep is a blanket term for any time a process spends suspended. Real-world time passes, but a process that isn't executing and won't see it. A clock that tracks sleep is still monotonic, it just has sudden jumps.

A few examples:

- [Doze on Android](https://developer.android.com/training/monitoring-device-state/doze-standby)
- Suspend to RAM on desktop platforms
- [Suspended apps on iOS](https://developer.apple.com/documentation/WatchKit/handling-common-state-transitions)

## Platform Support

- ✅ **Linux/Android**: `timerfd` with `CLOCK_MONOTONIC` (ignore sleep) or `CLOCK_BOOTTIME` (track sleep)
- ✅ **macOS/iOS**: `kqueue` + `EVFILT_TIMER` with mach absolute time (ignore sleep) or mach continuous time (track sleep)
- ~ **Windows**: I/O completion ports + high-resolution waitable timers (_incomplete_, sleep is always tracked)

## Async Runtime Support

`systime` supports both the `tokio` and `smol` async ecosystems via feature flags.
Tokio support is enabled by default.

## Use Cases

Tracking sleep is useful for:

- **Networking timers**: Any timeouts, keepalives, and other networking timers likely need to be aware of system sleep.
- **Authentication**: Credentials may expire sooner than expected unless system sleep is tracked.
- **User-facing scheduled tasks**: Run tasks at specific intervals regardless of system sleep (i.e. UI reminder popup).
- **Consistent cross-device timing**: When correlating events between devices, it's important to know the real world time that has passed.

Ignoring sleep is useful for:

- **Internal scheduled tasks**: Run tasks at specific intervals in accordance with time spent executing (i.e. garbage collection).
- **Performance monitoring / profiling**: Client-side metrics probably shouldn't track system sleep since it adds wild, inaccurate p99/max values to your dashboards.

## Performance

While every effort has been made to ensure timers are fast and efficient (i.e. sub-millisecond precision on Windows), this crate does use system timer facilities and there are limits on these resources. Hence why most operations can return an error.

Use these timers _sparingly_ and include a fallback to the appropriate equivalent (i.e. tokio::time for tokio) in case of failure. Alternatively, a [timer wheel](https://tokio.rs/blog/2018-03-timers)-esque approach could be used.
