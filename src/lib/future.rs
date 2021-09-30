use super::*;

struct WakerFn<F>(F);
impl<F: Fn() + Send + Sync> Wake for WakerFn<F> {
    fn wake(self: Arc<Self>) {
        self.0();
    }
}

pub fn waker_fn<F: Fn() + Send + Sync + 'static>(f: F) -> Waker {
    Waker::from(Arc::from(WakerFn(f)))
}

pub fn block_on<T>(future: impl Future<Output = T>) -> T {
    println!("block_on()");

    // Parker and unparker for notifying the current thread.
    let (_p, u) = parking::pair();

    // This boolean is set to `true` when the current thread is blocked on I/O.
    let io_blocked = Arc::new(AtomicBool::new(false));

    thread_local! {
        // Indicates that the current thread is polling I/O, but not necessarily blocked on it.
        static IO_POLLING: Cell<bool> = Cell::new(false);
    }

    // Prepare the waker.
    let waker = waker_fn({
        let io_blocked = io_blocked.clone();
        move || {
            if u.unpark() {
                // Check if waking from another thread and if currently blocked on I/O.
                if !IO_POLLING.with(Cell::get) && io_blocked.load(Ordering::SeqCst) {
                    Reactor::get().notify();
                }
            }
        }
    });

    let cx = &mut Context::from_waker(&waker);
    pin!(future);

    loop {
        println!("Polling...");

        // Poll the future.
        if let Poll::Ready(t) = future.as_mut().poll(cx) {
            println!("block_on: completed");
            return t;
        }

        // Try grabbing a lock on the reactor to wait on I/O.
        let mut reactor_lock = Reactor::get().lock();

        println!("block_on: waiting on I/O");
        reactor_lock.react().ok();
    }
}

#[derive(Debug)]
pub struct Async<T> {
    source: Arc<reactor::Source>,
    io: Option<T>,
}

impl<T> Unpin for Async<T> {}

impl<T: AsRawFd> Async<T> {
    pub fn new(io: T) -> io::Result<Async<T>> {
        let fd = io.as_raw_fd();

        // Put the file descriptor in non-blocking mode.
        unsafe {
            let mut res = libc::fcntl(fd, libc::F_GETFL);
            if res != -1 {
                res = libc::fcntl(fd, libc::F_SETFL, res | libc::O_NONBLOCK);
            }
            if res == -1 {
                return Err(std::io::Error::last_os_error());
            }
        }

        Ok(Async {
            source: Reactor::get().insert_io(fd)?,
            io: Some(io),
        })
    }
}

impl<T: AsRawFd> AsRawFd for Async<T> {
    fn as_raw_fd(&self) -> RawFd {
        self.source.raw
    }
}

impl<T> Async<T> {
    pub async fn read_with<R>(&self, op: impl FnMut(&T) -> io::Result<R>) -> io::Result<R> {
        let mut op = op;
        loop {
            match op(self.io.as_ref().unwrap()) {
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
                res => return res,
            }

            Readable(Ready {
                handle: self,
                dir: 0, /* READ */
                ticks: None,
                index: None,
                _guard: None,
            })
            .await?;
        }
    }
}

impl<T> AsRef<T> for Async<T> {
    fn as_ref(&self) -> &T {
        self.io.as_ref().unwrap()
    }
}

impl<T> AsMut<T> for Async<T> {
    fn as_mut(&mut self) -> &mut T {
        self.io.as_mut().unwrap()
    }
}

impl Async<TcpListener> {
    pub fn bind<A: Into<SocketAddr>>(addr: A) -> io::Result<Async<TcpListener>> {
        let addr = addr.into();
        Async::new(TcpListener::bind(addr)?)
    }

    pub async fn accept(&self) -> io::Result<(Async<TcpStream>, SocketAddr)> {
        let (stream, addr) = self.read_with(|io| io.accept()).await?;
        Ok((Async::new(stream)?, addr))
    }
}

struct Ready<H: Borrow<Async<T>>, T> {
    handle: H,
    dir: usize,
    ticks: Option<(usize, usize)>,
    index: Option<usize>,
    _guard: Option<RemoveOnDrop<T>>,
}

impl<H: Borrow<Async<T>>, T> Unpin for Ready<H, T> {}

impl<H: Borrow<Async<T>> + Clone, T> Future for Ready<H, T> {
    type Output = io::Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let Self {
            ref handle,
            dir,
            ticks,
            index,
            _guard,
            ..
        } = &mut *self;

        let mut state = handle.borrow().source.state.lock().unwrap();

        // Check if the reactor has delivered an event.
        if let Some((a, b)) = *ticks {
            // If `state[dir].tick` has changed to a value other than the old reactor tick,
            // that means a newer reactor tick has delivered an event.
            if state[*dir].tick != a && state[*dir].tick != b {
                return Poll::Ready(Ok(()));
            }
        }

        let was_empty = state[*dir].is_empty();

        // Register the current task's waker.
        let i = match *index {
            Some(i) => i,
            None => {
                let i = state[*dir].wakers.insert(None);
                *_guard = Some(RemoveOnDrop {
                    // handle: handle.clone(),
                    // dir: *dir,
                    // key: i,
                    _marker: PhantomData,
                });
                *index = Some(i);
                *ticks = Some((Reactor::get().ticker(), state[*dir].tick));
                i
            }
        };
        state[*dir].wakers[i] = Some(cx.waker().clone());

        // Update interest in this I/O handle.
        if was_empty {
            Reactor::get().poller.modify(
                handle.borrow().source.raw,
                Event {
                    key: handle.borrow().source.key,
                    readable: !state[READ].is_empty(),
                    writable: !state[WRITE].is_empty(),
                },
            )?;
        }

        Poll::Pending
    }
}

/// Remove waker when dropped.
struct RemoveOnDrop<T> {
    // handle: H,
    // dir: usize,
    // key: usize,
    _marker: PhantomData<fn() -> T>,
}

#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct Readable<'a, T>(Ready<&'a Async<T>, T>);

impl<T> Future for Readable<'_, T> {
    type Output = io::Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.0).poll(cx) {
            Poll::Ready(t) => t,
            Poll::Pending => return Poll::Pending,
        }?;

        println!("readable: fd={}", self.0.handle.source.raw);
        Poll::Ready(Ok(()))
    }
}
