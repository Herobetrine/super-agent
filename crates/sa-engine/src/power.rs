//! Keep the system awake while the engine owns running work.

#[cfg(any(windows, test))]
mod state {
    use std::time::{Duration, Instant};

    const IDLE_GRACE: Duration = Duration::from_secs(60);

    pub(super) trait PowerBackend {
        fn hold(&self);
        fn release(&self);
    }

    enum State {
        Released,
        Busy,
        Grace(Instant),
    }

    pub(super) struct KeepAwake<B, C> {
        backend: B,
        now: C,
        state: State,
    }

    impl<B: PowerBackend, C: Fn() -> Instant> KeepAwake<B, C> {
        pub(super) fn new(backend: B, now: C) -> Self {
            Self {
                backend,
                now,
                state: State::Released,
            }
        }

        pub(super) fn set_busy(&mut self, busy: bool) {
            match (&self.state, busy) {
                (State::Released, true) => {
                    self.backend.hold();
                    self.state = State::Busy;
                }
                (State::Grace(_), true) => self.state = State::Busy,
                (State::Busy, false) => self.state = State::Grace((self.now)() + IDLE_GRACE),
                (State::Grace(deadline), false) if (self.now)() >= *deadline => self.close(),
                _ => {}
            }
        }

        pub(super) fn idle_remaining(&self) -> Option<Duration> {
            match self.state {
                State::Grace(deadline) => Some(deadline.saturating_duration_since((self.now)())),
                _ => None,
            }
        }

        pub(super) fn close(&mut self) {
            if !matches!(self.state, State::Released) {
                self.backend.release();
                self.state = State::Released;
            }
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::state::{KeepAwake, PowerBackend};
    use std::{
        io,
        sync::{Arc, Mutex, MutexGuard, mpsc},
        thread::{self, JoinHandle},
        time::Instant,
    };
    use windows_sys::Win32::System::Power::{
        ES_CONTINUOUS, ES_SYSTEM_REQUIRED, EXECUTION_STATE, SetThreadExecutionState,
    };

    #[derive(Default)]
    pub(crate) struct Power(Arc<Mutex<Activity>>);

    pub(crate) struct BusyGuard(Arc<Mutex<Activity>>);

    #[derive(Default)]
    struct Activity {
        running: usize,
        sent_busy: bool,
        worker: Option<Worker>,
        closed: bool,
    }

    impl Power {
        pub(crate) fn track(&self) -> BusyGuard {
            let mut activity = lock(&self.0);
            activity.running += 1;
            activity.refresh();
            BusyGuard(self.0.clone())
        }

        pub(crate) fn refresh(&self) {
            lock(&self.0).refresh();
        }

        pub(crate) fn close(&self) {
            let mut activity = lock(&self.0);
            activity.closed = true;
            if let Some(worker) = activity.worker.take() {
                worker.close();
            }
        }
    }

    impl Drop for Power {
        fn drop(&mut self) {
            self.close();
        }
    }

    impl Drop for BusyGuard {
        fn drop(&mut self) {
            let mut activity = lock(&self.0);
            activity.running -= 1;
            activity.refresh();
        }
    }

    fn lock(activity: &Mutex<Activity>) -> MutexGuard<'_, Activity> {
        activity.lock().unwrap_or_else(|error| error.into_inner())
    }

    impl Activity {
        fn refresh(&mut self) {
            let busy = self.running != 0;
            if self.closed || busy == self.sent_busy {
                return;
            }
            if self.worker.is_none() && busy {
                match Worker::start() {
                    Ok(worker) => self.worker = Some(worker),
                    Err(error) => {
                        eprintln!("无法启动系统保活线程：{error}");
                        return;
                    }
                }
            }
            if self
                .worker
                .as_ref()
                .is_some_and(|worker| worker.tx.send(busy).is_ok())
            {
                self.sent_busy = busy;
            } else {
                self.worker = None;
                self.sent_busy = false;
            }
        }
    }

    struct Worker {
        tx: mpsc::Sender<bool>,
        thread: JoinHandle<()>,
    }

    impl Worker {
        fn start() -> io::Result<Self> {
            let (tx, rx) = mpsc::channel();
            let thread = thread::Builder::new()
                .name("sa-system-power".into())
                .spawn(move || {
                    let mut state = KeepAwake::new(WindowsBackend, Instant::now);
                    // A grace deadline wakes this thread once. Activity changes wake it
                    // immediately, including when the scheduler timer has stopped.
                    loop {
                        let next = match state.idle_remaining() {
                            Some(remaining) => rx.recv_timeout(remaining),
                            None => rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
                        };
                        match next {
                            Ok(busy) => state.set_busy(busy),
                            Err(mpsc::RecvTimeoutError::Timeout) => state.set_busy(false),
                            Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        }
                    }
                    state.close();
                })?;
            Ok(Self { tx, thread })
        }

        fn close(self) {
            // Disconnect before joining, keeping both Windows API calls on
            // the worker that owns the execution state.
            drop(self.tx);
            let _ = self.thread.join();
        }
    }

    struct WindowsBackend;

    impl PowerBackend for WindowsBackend {
        fn hold(&self) {
            set_execution_state(ES_CONTINUOUS | ES_SYSTEM_REQUIRED);
        }

        fn release(&self) {
            set_execution_state(ES_CONTINUOUS);
        }
    }

    fn set_execution_state(flags: EXECUTION_STATE) {
        // SAFETY: The flags are valid execution states and have no memory
        // requirements. This call only updates the calling worker thread.
        if unsafe { SetThreadExecutionState(flags) } == 0 {
            eprintln!("SetThreadExecutionState 无法设置系统保活状态 ({flags:#x})");
        }
    }
}

#[cfg(not(windows))]
mod platform {
    #[derive(Default)]
    pub(crate) struct Power {
        _private: (),
    }
    pub(crate) struct BusyGuard;

    impl Power {
        pub(crate) fn track(&self) -> BusyGuard {
            BusyGuard
        }

        pub(crate) fn refresh(&self) {}

        pub(crate) fn close(&self) {}
    }
}

pub(crate) use platform::{BusyGuard, Power};

#[cfg(test)]
mod tests {
    use super::state::{KeepAwake, PowerBackend};
    use std::{
        cell::{Cell, RefCell},
        time::{Duration, Instant},
    };

    #[derive(Debug, PartialEq, Eq)]
    enum Call {
        Hold,
        Release,
    }

    #[derive(Default)]
    struct FakeBackend(RefCell<Vec<Call>>);

    impl PowerBackend for &FakeBackend {
        fn hold(&self) {
            self.0.borrow_mut().push(Call::Hold);
        }

        fn release(&self) {
            self.0.borrow_mut().push(Call::Release);
        }
    }

    #[test]
    fn holds_once_and_releases_only_after_sixty_seconds_idle() {
        let backend = FakeBackend::default();
        let now = Cell::new(Instant::now());
        let mut state = KeepAwake::new(&backend, || now.get());
        state.set_busy(false);
        assert!(backend.0.borrow().is_empty());
        state.set_busy(true);
        assert_eq!(*backend.0.borrow(), [Call::Hold]);
        state.set_busy(true);
        assert_eq!(*backend.0.borrow(), [Call::Hold]);

        state.set_busy(false);
        assert_eq!(state.idle_remaining(), Some(Duration::from_secs(60)));
        now.set(now.get() + Duration::from_millis(59_999));
        state.set_busy(false);
        assert_eq!(*backend.0.borrow(), [Call::Hold]);
        now.set(now.get() + Duration::from_millis(1));
        state.set_busy(false);
        assert_eq!(*backend.0.borrow(), [Call::Hold, Call::Release]);
        assert_eq!(state.idle_remaining(), None);
    }

    #[test]
    fn more_work_cancels_the_old_idle_deadline() {
        let backend = FakeBackend::default();
        let now = Cell::new(Instant::now());
        let mut state = KeepAwake::new(&backend, || now.get());
        state.set_busy(true);
        state.set_busy(false);
        now.set(now.get() + Duration::from_secs(30));
        state.set_busy(true);
        assert_eq!(state.idle_remaining(), None);
        state.set_busy(false);

        now.set(now.get() + Duration::from_secs(30));
        state.set_busy(false);
        assert_eq!(*backend.0.borrow(), [Call::Hold]);
        now.set(now.get() + Duration::from_secs(30));
        state.set_busy(false);
        assert_eq!(*backend.0.borrow(), [Call::Hold, Call::Release]);
    }

    #[test]
    fn close_releases_immediately_and_is_idempotent() {
        for in_grace in [false, true] {
            let backend = FakeBackend::default();
            let now = Instant::now();
            let mut state = KeepAwake::new(&backend, || now);
            state.set_busy(true);
            if in_grace {
                state.set_busy(false);
            }
            state.close();
            assert_eq!(*backend.0.borrow(), [Call::Hold, Call::Release]);
            state.close();
            state.set_busy(false);
            assert_eq!(*backend.0.borrow(), [Call::Hold, Call::Release]);
        }
    }
}
