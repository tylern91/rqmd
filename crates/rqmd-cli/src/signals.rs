//! Catch SIGINT, SIGTERM and SIGHUP so a command can stop at a safe point
//! instead of being killed mid-write. The handler only records the signal;
//! callers poll [`pending`] between units of work.

#[cfg(unix)]
mod imp {
    use std::cell::Cell;
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
    use std::sync::{Mutex, MutexGuard};

    use crate::exit::EXIT_SIGNAL_BASE;

    const CAUGHT: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

    /// The last signal received, or 0. Set by the handler.
    static RECEIVED: AtomicI32 = AtomicI32::new(0);
    /// How many signals the handler has seen since the outermost [`Catch`] was installed.
    static COUNT: AtomicU32 = AtomicU32::new(0);
    /// When set, the handler ends the process on the second signal.
    static EXIT_ON_REPEAT: AtomicBool = AtomicBool::new(false);
    /// One thread at a time owns the process-wide handlers.
    static OWNER: Mutex<()> = Mutex::new(());

    thread_local! {
        static HELD: Cell<bool> = const { Cell::new(false) };
    }

    extern "C" fn on_signal(signal: libc::c_int) {
        RECEIVED.store(signal, Ordering::SeqCst);
        let earlier = COUNT.fetch_add(1, Ordering::SeqCst);
        if earlier > 0 && EXIT_ON_REPEAT.load(Ordering::SeqCst) {
            // SAFETY: `_exit` is async-signal-safe; nothing is flushed or dropped.
            unsafe { libc::_exit(i32::from(EXIT_SIGNAL_BASE) + signal) };
        }
    }

    /// The handlers to restore, held by the outermost [`Catch`] of a thread.
    struct Outer {
        previous: Vec<(libc::c_int, libc::sigaction)>,
        _owner: MutexGuard<'static, ()>,
    }

    impl Outer {
        fn install() -> Self {
            let owner = OWNER.lock().unwrap_or_else(|e| e.into_inner());
            RECEIVED.store(0, Ordering::SeqCst);
            COUNT.store(0, Ordering::SeqCst);
            let mut previous = Vec::new();
            for signal in CAUGHT {
                // SAFETY: `sigaction` is plain C data; all-zero is a valid empty value.
                let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
                // SAFETY: a null new action only reads the current one into `old`.
                if unsafe { libc::sigaction(signal, std::ptr::null(), &mut old) } != 0 {
                    continue;
                }
                // A signal the parent ignored (`nohup`, a background job) stays ignored.
                if old.sa_sigaction == libc::SIG_IGN {
                    continue;
                }
                // SAFETY: as above.
                let mut new: libc::sigaction = unsafe { std::mem::zeroed() };
                new.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
                // A signal that lands during a read or write resumes it instead of
                // failing it with EINTR.
                new.sa_flags = libc::SA_RESTART;
                // SAFETY: `new.sa_mask` is a valid sigset to initialise.
                unsafe { libc::sigemptyset(&mut new.sa_mask) };
                // SAFETY: the handler only touches atomics and calls `_exit`, both
                // async-signal-safe.
                if unsafe { libc::sigaction(signal, &new, std::ptr::null_mut()) } == 0 {
                    previous.push((signal, old));
                }
            }
            Self {
                previous,
                _owner: owner,
            }
        }
    }

    impl Drop for Outer {
        fn drop(&mut self) {
            for (signal, old) in &self.previous {
                // SAFETY: `old` is the action `sigaction` reported for this signal.
                unsafe { libc::sigaction(*signal, old, std::ptr::null_mut()) };
            }
        }
    }

    /// Catches SIGINT, SIGTERM and SIGHUP until dropped, then restores the
    /// previous handlers. Guards nest on one thread; the outermost owns the
    /// handlers and each guard restores the repeat behaviour it replaced.
    pub struct Catch {
        outer: Option<Outer>,
        previous_repeat: bool,
    }

    impl Catch {
        /// Record signals only. A hook runner uses this: it decides what a repeat means.
        pub fn install() -> Self {
            Self::with_repeat(false)
        }

        /// Record the first signal; a second one ends the process at once with
        /// `128 + n`.
        pub fn install_exiting_on_repeat() -> Self {
            Self::with_repeat(true)
        }

        fn with_repeat(exit_on_repeat: bool) -> Self {
            let outer = if HELD.with(Cell::get) {
                None
            } else {
                HELD.with(|held| held.set(true));
                Some(Outer::install())
            };
            let previous_repeat = EXIT_ON_REPEAT.swap(exit_on_repeat, Ordering::SeqCst);
            Self {
                outer,
                previous_repeat,
            }
        }
    }

    impl Drop for Catch {
        fn drop(&mut self) {
            EXIT_ON_REPEAT.store(self.previous_repeat, Ordering::SeqCst);
            if self.outer.is_some() {
                HELD.with(|held| held.set(false));
                RECEIVED.store(0, Ordering::SeqCst);
            }
        }
    }

    /// The last signal received since the outermost guard was installed. A thread
    /// without a guard sees none, so a signal raised under another thread's guard
    /// cannot stop unrelated work.
    pub fn pending() -> Option<i32> {
        if !HELD.with(Cell::get) {
            return None;
        }
        match RECEIVED.load(Ordering::SeqCst) {
            0 => None,
            signal => Some(signal),
        }
    }

    /// How many signals have been received since the outermost guard was installed.
    pub fn count() -> u32 {
        COUNT.load(Ordering::SeqCst)
    }
}

#[cfg(not(unix))]
mod imp {
    pub struct Catch;

    impl Catch {
        pub fn install() -> Self {
            Self
        }

        pub fn install_exiting_on_repeat() -> Self {
            Self
        }
    }

    pub fn pending() -> Option<i32> {
        None
    }

    pub fn count() -> u32 {
        0
    }
}

pub use imp::{Catch, count, pending};
