use core::{
    borrow::Borrow,
    cell::Cell,
    future::Future,
    marker::PhantomData,
    pin::Pin,
    sync::atomic::AtomicUsize,
    sync::atomic::{AtomicBool, Ordering},
    task::{Context, Poll, Waker},
};

extern crate alloc;
use alloc::{sync::Arc, task::Wake};

use std::{
    io,
    net::{SocketAddr, TcpListener, TcpStream},
    os::unix::prelude::{AsRawFd, RawFd},
    sync::{Mutex, MutexGuard},
};

use once_cell::sync::Lazy;
use polling::{Event, Poller};
use slab::Slab;

pub const READ: usize = 0;
pub const WRITE: usize = 1;

#[macro_export]
macro_rules! pin {
    ($($x:ident),* $(,)?) => {
        $(
            let mut $x = $x;
            #[allow(unused_mut)]
            let mut $x = unsafe {
                core::pin::Pin::new_unchecked(&mut $x)
            };
        )*
    }
}

pub mod future;
pub mod reactor;

pub use future::Async;
pub use reactor::Reactor;
