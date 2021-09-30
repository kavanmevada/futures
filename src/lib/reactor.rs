use super::*;

pub struct Reactor {
    pub(crate) poller: Poller,
    ticker: AtomicUsize,
    sources: Mutex<Slab<Arc<Source>>>,
    events: Mutex<Vec<Event>>,
}

impl Reactor {
    /// Returns a reference to the reactor.
    pub(crate) fn get() -> &'static Reactor {
        static REACTOR: Lazy<Reactor> = Lazy::new(|| {
            //crate::driver::init();
            Reactor {
                poller: Poller::new().expect("cannot initialize I/O event notification"),
                ticker: AtomicUsize::new(0),
                sources: Mutex::new(Slab::new()),
                events: Mutex::new(Vec::new()),
            }
        });
        &REACTOR
    }

    /// Returns the current ticker.
    pub(crate) fn ticker(&self) -> usize {
        self.ticker.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub(crate) fn lock(&self) -> ReactorLock<'_> {
        let reactor = self;
        let events = self.events.lock().unwrap();
        ReactorLock { reactor, events }
    }

    /// Notifies the thread blocked on the reactor.
    pub(crate) fn notify(&self) {
        self.poller.notify().expect("failed to notify reactor");
    }

    pub(crate) fn insert_io(&self, raw: RawFd) -> io::Result<Arc<Source>> {
        let source = {
            let mut sources = self.sources.lock().unwrap();
            let key = sources.vacant_entry().key();
            let source = Arc::new(Source {
                raw,
                key,
                state: Default::default(),
            });
            sources.insert(source.clone());
            source
        };

        // Register the file descriptor.
        if let Err(err) = self.poller.add(raw, Event::none(source.key)) {
            let mut sources = self.sources.lock().unwrap();
            sources.remove(source.key);
            return Err(err);
        }

        Ok(source)
    }
}

pub(crate) struct ReactorLock<'a> {
    reactor: &'a Reactor,
    events: MutexGuard<'a, Vec<Event>>,
}

impl ReactorLock<'_> {
    /// Processes new events, blocking until the first event or the timeout.
    pub(crate) fn react(&mut self) -> io::Result<()> {
        let mut wakers = Vec::new();

        let tick = self
            .reactor
            .ticker
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            .wrapping_add(1);

        self.events.clear();

        // Block on I/O events.
        let res = match self.reactor.poller.wait(&mut self.events, None) {
            Ok(0) => Ok(()),

            // At least one I/O event occurred.
            Ok(_) => {
                // Iterate over sources in the event list.
                let sources = self.reactor.sources.lock().unwrap();

                for ev in self.events.iter() {
                    // Check if there is a source in the table with this key.
                    if let Some(source) = sources.get(ev.key) {
                        let mut state = source.state.lock().unwrap();

                        // Collect wakers if a writability event was emitted.
                        for &(dir, emitted) in &[(WRITE, ev.writable), (READ, ev.readable)] {
                            if emitted {
                                state[dir].tick = tick;
                                state[dir].drain_into(&mut wakers);
                            }
                        }

                        // Re-register if there are still writers or readers. The can happen if
                        // e.g. we were previously interested in both readability and writability,
                        // but only one of them was emitted.
                        if !state[READ].is_empty() || !state[WRITE].is_empty() {
                            self.reactor.poller.modify(
                                source.raw,
                                Event {
                                    key: source.key,
                                    readable: !state[READ].is_empty(),
                                    writable: !state[WRITE].is_empty(),
                                },
                            )?;
                        }
                    }
                }

                Ok(())
            }

            // The syscall was interrupted.
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => Ok(()),

            // An actual error occureed.
            Err(err) => Err(err),
        };

        for waker in wakers {
            // Don't let a panicking waker blow everything up.
            waker.wake();
        }

        res
    }
}

#[derive(Debug, Default)]
pub struct Direction {
    pub(crate) tick: usize,
    pub(crate) waker: Option<Waker>,
    pub(crate) wakers: Slab<Option<Waker>>,
}

#[derive(Debug)]
pub struct Source {
    pub(crate) raw: RawFd,
    pub(crate) key: usize,
    pub(crate) state: Mutex<[Direction; 2]>,
}

impl Direction {
    /// Returns `true` if there are no wakers interested in this direction.
    pub fn is_empty(&self) -> bool {
        self.waker.is_none() && self.wakers.iter().all(|(_, opt)| opt.is_none())
    }

    /// Moves all wakers into a `Vec`.
    fn drain_into(&mut self, dst: &mut Vec<Waker>) {
        if let Some(w) = self.waker.take() {
            dst.push(w);
        }
        for (_, opt) in self.wakers.iter_mut() {
            if let Some(w) = opt.take() {
                dst.push(w);
            }
        }
    }
}
