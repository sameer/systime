# systime

 A Rust crate for portable timers that handle system sleep consistently.

## Motivation

Timers in std, tokio, and generally every Rust library behave differently depending on the platform:

- **Linux/Android**: `CLOCK_MONOTONIC` excludes time spent sleeping
- **macOS/iOS**: `CLOCK_UPTIME_RAW` (or `mach_absolute_time`) excludes time spent sleeping
- **Windows**: `QueryPerformanceCounter` *includes* time spent sleeping ([rust-lang/rust#79462](https://github.com/rust-lang/rust/issues/79462))

This makes it difficult to write portable code with predictable timer behavior, especially in applications that need to account for the real-world time that has passed (including system sleep).

`systime` offers a simple API where you explicitly choose whether to track or ignore system sleep.

## Platform Support

- **Linux/Android**: `timerfd` with `CLOCK_MONOTONIC` (ignore sleep) or `CLOCK_BOOTTIME` (track sleep)
- **macOS/iOS**: `kqueue` + `EVFILT_TIMER` with mach absolute time (ignore sleep) or mach continuous time (track sleep)
- **Windows**: I/O completion ports + high-resolution waitable timers (_incomplete_, sleep is always tracked)

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
- **Performance monitoring / profiling**: Benchmarks should not track system sleep, because only execution time matters.

## Performance

This library uses system OS timer facilities. There are limits on these resources, hence why most operations can return an error.
Use these timers _sparingly_ and include a fallback to the appropriate equivalent (i.e. tokio::time for tokio) in case of failure.
