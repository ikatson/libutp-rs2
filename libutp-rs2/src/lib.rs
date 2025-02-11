mod traits;

use std::{collections::HashMap, net::SocketAddr, sync::Arc};

use futures::task::AtomicWaker;
use libutp_rs2_sys::{
    uint64, utp_callback_arguments, utp_create_socket, utp_get_userdata, utp_socket,
};
use parking_lot::Mutex;
use traits::Transport;

/// Arch
/// UtpContext
/// - has the transport. Others don't.
///
/// UtpStream
/// - ptr to socket? yes, we need it to call "write()"
/// - userdata? Just recursive Arc<ReentrantMutex<self>>? Yes, why not.
///
/// UtpStream.write()
/// - gets the global lock. Tries to write
///   - if written something - Ready(Ok(()))
///   - if written nothing - register waker.
///     When we get called again on "UTP_STATE_WRITABLE", wake up the waker
///
/// Callbacks are global.
/// - on_state_change
///   - on_connect/on_writeable:
///     - this is probably called from within ANY function like write() etc. We need either a re-entrant lock,
///       or we need to unsafely assume the lock is already taken. The first is easier.
///     - get the user data from socket. It should contain the map

static LOCK: Mutex<()> = Mutex::new(());

pub fn with_global_lock<R>(f: impl FnOnce() -> R) -> R {
    let _g = LOCK.lock();
    f()
}

struct ContextData {
    sockets: HashMap<*const utp_socket, Arc<UtpStream>>,
}

unsafe extern "C" fn utp_on_writeable<T>(arg1: *mut utp_callback_arguments) -> uint64 {
    let args = arg1.as_mut().unwrap();
    let data = utp_get_userdata(args.socket) as SocketUserData;
    let data = data.as_ref().unwrap();
    data.writeable_waker.wake();

    // TODO: what to return?
    0
}

type SocketUserData = *const UtpStream;

pub struct UtpContext<T> {
    transport: T,

    // TODO: store hidden pointer that can only be looked at with the global lock
    ctx: *mut libutp_rs2_sys::utp_context,
}

pub struct UtpStream {
    writeable_waker: AtomicWaker,
}

impl<T: Transport> UtpContext<T> {
    pub fn new(transport: T) -> Option<Self> {
        let ctx = unsafe { libutp_rs2_sys::utp_init(2) };
        if ctx.is_null() {
            return None;
        }
        Some(Self { transport, ctx })
    }

    pub async fn connect(&self, addr: SocketAddr) -> Option<UtpStream> {
        let sock = unsafe { utp_create_socket(self.ctx) };
        if sock.is_null() {
            return None;
        }

        let stream = Arc::new(UtpStream {
            writeable_waker: Default::default(),
        });

        // TODO: check if this worked
        utp_set_userdata(sock, Arc::into_raw(stream.clone()));

        todo!()

        // we create a socket
        // we call "connect", then wait
        //
        // there will be a callback with our socket
        //
        // the "on_connected" will notify us with the socket matching
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use libutp_rs2_sys::{utp_connect, utp_set_callback, UTP_ON_CONNECT};

    #[tokio::test]
    async fn test_init_basic() {
        let ctx = unsafe { libutp_rs2_sys::utp_init(2) };
        assert!(!ctx.is_null());

        let sock = unsafe { libutp_rs2_sys::utp_create_socket(ctx) };
        assert!(!sock.is_null());

        let listen = SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::LOCALHOST), 5001);

        let listen_os = os_socketaddr::OsSocketAddr::from(listen);

        unsafe {
            utp_connect(
                sock,
                listen_os.as_ptr() as *const libutp_rs2_sys::sockaddr,
                listen_os.len(),
            )
        };

        // utp_set_callback(ctx, UTP_ON_CONNECT, proc_);

        todo!()
    }
}
