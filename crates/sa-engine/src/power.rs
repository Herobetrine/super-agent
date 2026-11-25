//! Keep the system awake while the engine owns running work.

use std::{future::Future, sync::Arc};
use tokio::task::JoinHandle;

#[cfg(any(windows, test))]
mod state {
    use std::time::{Duration, Instant};

    const IDLE_GRACE: Duration = Duration::from_secs(60);
    const RETRY_DELAY: Duration = Duration::from_secs(5);

    pub(super) trait PowerBackend {
        fn hold(&self) -> bool;
        fn release(&self) -> bool;
    }

    pub(super) struct KeepAwake<B, C> {
        backend: B,
        now: C,
        busy: bool,
        held: bool,
        idle_until: Option<Instant>,
        retry_at: Option<Instant>,
    }

    impl<B: PowerBackend, C: Fn() -> Instant> KeepAwake<B, C> {
        pub(super) fn new(backend: B, now: C) -> Self {
            Self {
                backend,
                now,
                busy: false,
                held: false,
                idle_until: None,
                retry_at: None,
            }
        }

        pub(super) fn set_busy(&mut self, busy: bool) {
            if busy != self.busy {
                self.busy = busy;
                self.idle_until = (!busy).then(|| (self.now)() + IDLE_GRACE);
            }
            self.update();
        }

        pub(super) fn update(&mut self) {
            if self.next_wake() == Some(Duration::ZERO) {
                self.apply(self.busy);
            }
        }

        pub(super) fn next_wake(&self) -> Option<Duration> {
            let now = (self.now)();
            let deadline = match (self.busy, self.held) {
                (true, false) => now,
                (false, true) => self.idle_until.unwrap_or(now),
                _ => return None,
            };
            Some(
                deadline
                    .max(self.retry_at.unwrap_or(now))
                    .saturating_duration_since(now),
            )
        }

        pub(super) fn close(&mut self) {
            self.busy = false;
            self.idle_until = None;
            if self.held {
                self.apply(false);
            }
        }

        fn apply(&mut self, hold: bool) {
            let confirmed = if hold {
                self.backend.hold()
            } else {
                self.backend.release()
            };
            if confirmed {
                self.held = hold;
                self.retry_at = None;
            } else {
                self.retry_at = Some((self.now)() + RETRY_DELAY);
            }
        }

        #[cfg(test)]
        pub(super) fn held(&self) -> bool {
            self.held
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

        #[cfg(test)]
        pub(crate) fn for_test() -> Self {
            // Exercise the actual guard counter without asking Windows to stay awake.
            Self(Arc::new(Mutex::new(Activity {
                closed: true,
                ..Activity::default()
            })))
        }

        #[cfg(test)]
        pub(crate) fn running(&self) -> usize {
            lock(&self.0).running
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
                    // Activity changes, idle deadlines and retries wake this thread
                    // even when the scheduler timer has stopped.
                    loop {
                        let next = match state.next_wake() {
                            Some(remaining) => rx.recv_timeout(remaining),
                            None => rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
                        };
                        match next {
                            Ok(busy) => state.set_busy(busy),
                            Err(mpsc::RecvTimeoutError::Timeout) => state.update(),
                            Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        }
                    }
                    // Closing always attempts an immediate release. Windows
                    // clears this thread's hold on exit even if the call fails.
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
        fn hold(&self) -> bool {
            set_execution_state(ES_CONTINUOUS | ES_SYSTEM_REQUIRED)
        }

        fn release(&self) -> bool {
            set_execution_state(ES_CONTINUOUS)
        }
    }

    fn set_execution_state(flags: EXECUTION_STATE) -> bool {
        // SAFETY: The flags are valid execution states and have no memory
        // requirements. This call only updates the calling worker thread.
        let confirmed = unsafe { SetThreadExecutionState(flags) } != 0;
        if !confirmed {
            eprintln!("SetThreadExecutionState 无法设置系统保活状态 ({flags:#x})");
        }
        confirmed
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

tokio::task_local! {
    static WORK_POWER: Arc<BusyGuard>;
}

impl BusyGuard {
    pub(crate) async fn scope<T>(self, work: impl Future<Output = T>) -> T {
        WORK_POWER.scope(Arc::new(self), work).await
    }

    /// Pass the current guard to a child that can outlive the awaiting task.
    pub(crate) fn inherit() -> Option<Arc<Self>> {
        WORK_POWER.try_with(Arc::clone).ok()
    }
}

impl Power {
    pub(crate) fn spawn_blocking<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> JoinHandle<T> {
        let power = self.track();
        tokio::task::spawn_blocking(move || {
            let _power = power;
            work()
        })
    }
}

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
    struct FakeBackend {
        calls: RefCell<Vec<Call>>,
        fail_hold: Cell<bool>,
        fail_release: Cell<bool>,
    }

    impl PowerBackend for &FakeBackend {
        fn hold(&self) -> bool {
            self.calls.borrow_mut().push(Call::Hold);
            !self.fail_hold.replace(false)
        }

        fn release(&self) -> bool {
            self.calls.borrow_mut().push(Call::Release);
            !self.fail_release.replace(false)
        }
    }

    #[test]
    fn holds_once_and_releases_only_after_sixty_seconds_idle() {
        let backend = FakeBackend::default();
        let now = Cell::new(Instant::now());
        let mut state = KeepAwake::new(&backend, || now.get());
        state.set_busy(false);
        assert!(backend.calls.borrow().is_empty());
        state.set_busy(true);
        assert_eq!(*backend.calls.borrow(), [Call::Hold]);
        state.set_busy(true);
        assert_eq!(*backend.calls.borrow(), [Call::Hold]);

        state.set_busy(false);
        assert_eq!(state.next_wake(), Some(Duration::from_secs(60)));
        now.set(now.get() + Duration::from_millis(59_999));
        state.set_busy(false);
        assert_eq!(*backend.calls.borrow(), [Call::Hold]);
        now.set(now.get() + Duration::from_millis(1));
        state.set_busy(false);
        assert_eq!(*backend.calls.borrow(), [Call::Hold, Call::Release]);
        assert_eq!(state.next_wake(), None);
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
        assert_eq!(state.next_wake(), None);
        state.set_busy(false);

        now.set(now.get() + Duration::from_secs(30));
        state.set_busy(false);
        assert_eq!(*backend.calls.borrow(), [Call::Hold]);
        now.set(now.get() + Duration::from_secs(30));
        state.set_busy(false);
        assert_eq!(*backend.calls.borrow(), [Call::Hold, Call::Release]);
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
            assert_eq!(*backend.calls.borrow(), [Call::Hold, Call::Release]);
            state.close();
            state.set_busy(false);
            assert_eq!(*backend.calls.borrow(), [Call::Hold, Call::Release]);
        }
    }

    #[test]
    fn failed_hold_and_release_retry_before_recording_success() {
        let backend = FakeBackend {
            fail_hold: Cell::new(true),
            fail_release: Cell::new(true),
            ..FakeBackend::default()
        };
        let now = Cell::new(Instant::now());
        let mut state = KeepAwake::new(&backend, || now.get());
        let retry = Duration::from_secs(5);

        state.set_busy(true);
        assert!(!state.held());
        assert_eq!(state.next_wake(), Some(retry));
        state.update();
        state.set_busy(true);
        assert_eq!(*backend.calls.borrow(), [Call::Hold]);
        now.set(now.get() + retry);
        state.update();
        assert!(state.held());
        assert_eq!(state.next_wake(), None);

        state.set_busy(false);
        now.set(now.get() + Duration::from_secs(60));
        state.update();
        assert!(state.held());
        assert_eq!(state.next_wake(), Some(retry));
        state.update();
        assert_eq!(
            *backend.calls.borrow(),
            [Call::Hold, Call::Hold, Call::Release]
        );
        now.set(now.get() + retry);
        state.update();
        assert!(!state.held());
        assert_eq!(state.next_wake(), None);
        assert_eq!(
            *backend.calls.borrow(),
            [Call::Hold, Call::Hold, Call::Release, Call::Release]
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn a_panicking_task_drops_its_busy_guard() {
        let power = super::Power::for_test();
        let guard = power.track();
        assert_eq!(power.running(), 1);
        let task = tokio::spawn(async move {
            let _power = guard;
            panic!("test task");
        });
        assert!(task.await.unwrap_err().is_panic());
        assert_eq!(power.running(), 0);
    }
}
