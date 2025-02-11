mod traits;

use std::{
    collections::VecDeque,
    ffi::{c_char, c_int, CStr},
    future::Future,
    marker::PhantomData,
    net::SocketAddr,
    sync::{atomic::AtomicBool, Arc},
    task::Poll,
    time::Duration,
};

use anyhow::bail;
use futures::task::AtomicWaker;
use libutp_rs2_sys::{
    uint64, utp_callback_arguments, utp_check_timeouts, utp_close, utp_connect,
    utp_context_get_userdata, utp_context_set_userdata, utp_create_socket, utp_destroy,
    utp_error_code_names, utp_get_userdata, utp_issue_deferred_acks, utp_process_udp,
    utp_read_drained, utp_set_callback, utp_set_userdata, utp_socket, utp_state_names, utp_write,
    UTP_ON_ACCEPT, UTP_ON_CONNECT, UTP_ON_ERROR, UTP_ON_READ, UTP_ON_STATE_CHANGE, UTP_SENDTO,
    UTP_STATE_CONNECT, UTP_STATE_DESTROYING, UTP_STATE_EOF, UTP_STATE_WRITABLE,
};
use os_socketaddr::OsSocketAddr;
use parking_lot::{Mutex, ReentrantMutex};
use ringbuf::{
    storage::Heap,
    traits::{Consumer, Producer},
    LocalRb,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::{trace, warn};
use traits::Transport;

static LOCK: ReentrantMutex<()> = ReentrantMutex::new(());

pub fn with_global_lock<R>(f: impl FnOnce() -> R) -> R {
    let _g = LOCK.lock();
    f()
}

unsafe extern "C" fn utp_on_connect<T>(args: *mut utp_callback_arguments) -> uint64 {
    trace!("utp_on_connect");
    let args = args.as_mut().unwrap();
    let data = utp_get_userdata(args.socket) as SocketUserData<T>;
    let data = data.as_ref().unwrap();
    data.writeable_waker.wake();

    // TODO: what to return?
    0
}

unsafe extern "C" fn utp_on_read<T>(args: *mut utp_callback_arguments) -> uint64 {
    trace!("utp_on_read");
    let args = args.as_mut().unwrap();
    let data = utp_get_userdata(args.socket) as SocketUserData<T>;
    let data = data.as_ref().unwrap();

    let inbuf = std::slice::from_raw_parts(args.buf, args.len);
    let mut buf = data.buffer.lock();
    buf.push_slice(inbuf);
    utp_read_drained(args.socket);
    // info!("utp_read_drained, utp_issue_deferred_acks");

    data.readable_waker.wake();
    0
}

unsafe extern "C" fn utp_on_sendto<T: Transport>(args: *mut utp_callback_arguments) -> uint64 {
    trace!("utp_on_sendto");
    let args = args.as_mut().unwrap();

    let addr = args.__bindgen_anon_1.address;
    let addr = OsSocketAddr::copy_from_raw(addr.cast(), args.__bindgen_anon_2.address_len)
        .into_addr()
        .unwrap();
    let udata: *const UtpContext<T> = utp_context_get_userdata(args.context).cast();
    let udata = udata.as_ref().unwrap();
    let buf = core::slice::from_raw_parts(args.buf, args.len);
    let sz = udata.transport.try_send_to(buf, addr);
    match sz {
        Ok(0) => warn!("sent 0"),
        Ok(_) => {}
        Err(e) => warn!("send error: {e:#}"),
    }

    0
}

unsafe extern "C" fn utp_on_error<T: Transport>(args: *mut utp_callback_arguments) -> uint64 {
    trace!("utp_on_error");
    let args = args.as_mut().unwrap();
    let error = args.__bindgen_anon_1.error_code;
    #[allow(static_mut_refs)]
    let error = get_name(utp_error_code_names.as_ptr(), error);
    warn!("utp_on_error: {error:?}");
    0
}

unsafe fn get_name(arr: *const *const c_char, name: i32) -> &'static str {
    let cptr = *arr.offset(name as isize);
    CStr::from_ptr(cptr).to_str().unwrap()
}

unsafe extern "C" fn utp_on_accept<T: Transport>(args: *mut utp_callback_arguments) -> uint64 {
    let args = args.as_mut().unwrap();

    let ctx = utp_context_get_userdata(args.context);
    let ctx: *const UtpContext<T> = ctx.cast();
    let ctx = Arc::from_raw(ctx);
    if let Some(acc) = ctx.accept_queue.lock().pop_front() {
        let stream = UtpStream::new(args.socket, ctx.clone());
        let _ = acc.send(stream);
    }
    std::mem::forget(ctx);
    0
}

unsafe extern "C" fn utp_on_state_change<T>(args: *mut utp_callback_arguments) -> uint64 {
    let args = args.as_mut().unwrap();
    let data = utp_get_userdata(args.socket) as SocketUserData<T>;
    let data = data.as_ref().unwrap();

    let state = args.__bindgen_anon_1.state;
    #[allow(static_mut_refs)]
    let state_name = get_name(utp_state_names.as_ptr(), state);
    trace!("state: {:?}", state_name);

    match state as u32 {
        UTP_STATE_EOF => {
            data.is_eof.store(true, std::sync::atomic::Ordering::SeqCst);
            data.readable_waker.wake();
        }
        UTP_STATE_CONNECT => {
            data.writeable_waker.wake();
        }
        UTP_STATE_WRITABLE => {
            data.writeable_waker.wake();
        }
        UTP_STATE_DESTROYING => {
            data.is_eof.store(true, std::sync::atomic::Ordering::SeqCst);
            data.readable_waker.wake();
            data.writeable_waker.wake();
        }
        _ => {
            todo!()
        }
    };

    data.writeable_waker.wake();
    0
}

type SocketUserData<T> = *const UtpStreamInner<T>;

pub struct UtpContext<T> {
    transport: T,

    accept_queue: Mutex<VecDeque<tokio::sync::oneshot::Sender<UtpStream<T>>>>,

    // TODO: store hidden pointer that can only be looked at with the global lock
    ctx: *mut libutp_rs2_sys::utp_context,
}

impl<T> Drop for UtpContext<T> {
    fn drop(&mut self) {
        unsafe {
            with_global_lock(|| {
                utp_destroy(self.ctx);
            })
        }
    }
}

unsafe impl<T: Send> Send for UtpContext<T> {}
unsafe impl<T: Send> Sync for UtpContext<T> {}

struct UtpStreamInner<T> {
    buffer: Mutex<LocalRb<Heap<u8>>>,
    is_eof: AtomicBool,
    readable_waker: AtomicWaker,
    writeable_waker: AtomicWaker,
    utp_socket: *mut utp_socket,
    _t: PhantomData<T>,
}

unsafe impl<T: Send> Send for UtpStreamInner<T> {}
unsafe impl<T: Send> Sync for UtpStreamInner<T> {}

impl<T> Default for UtpStreamInner<T> {
    fn default() -> Self {
        Self {
            buffer: Mutex::new(LocalRb::new(1024 * 1024)),
            is_eof: Default::default(),
            readable_waker: Default::default(),
            writeable_waker: Default::default(),
            utp_socket: core::ptr::null_mut(),
            _t: Default::default(),
        }
    }
}

pub struct UtpStream<T> {
    inner: Box<UtpStreamInner<T>>,
    ctx: Arc<UtpContext<T>>,
}

impl<T> UtpStream<T> {
    fn new(sock: *mut utp_socket, ctx: Arc<UtpContext<T>>) -> Self {
        assert!(!sock.is_null());
        let s = UtpStream {
            inner: Box::new(UtpStreamInner {
                utp_socket: sock,
                ..Default::default()
            }),
            ctx,
        };
        let inner_ptr: *const UtpStreamInner<T> = s.inner.as_ref();
        unsafe { utp_set_userdata(sock, inner_ptr.cast_mut().cast()) };
        s
    }

    fn get_context(&self) -> &UtpContext<T> {
        &self.ctx
    }
}

impl<T> Drop for UtpStream<T> {
    fn drop(&mut self) {
        unsafe {
            utp_close(self.inner.utp_socket);
        }
    }
}

impl UtpUdpContext {
    pub async fn new_udp(bind_addr: SocketAddr) -> Option<Arc<Self>> {
        let sock = tokio::net::UdpSocket::bind(bind_addr).await.unwrap();
        Self::new(sock)
    }
}

impl<T: Transport> UtpContext<T> {
    pub fn new(transport: T) -> Option<Arc<Self>> {
        let ctx = unsafe { libutp_rs2_sys::utp_init(2) };
        if ctx.is_null() {
            return None;
        }

        unsafe { utp_set_callback(ctx, UTP_ON_CONNECT as c_int, Some(utp_on_connect::<T>)) };
        unsafe {
            utp_set_callback(
                ctx,
                UTP_ON_STATE_CHANGE as c_int,
                Some(utp_on_state_change::<T>),
            )
        };
        unsafe { utp_set_callback(ctx, UTP_ON_READ as c_int, Some(utp_on_read::<T>)) };
        unsafe { utp_set_callback(ctx, UTP_SENDTO as c_int, Some(utp_on_sendto::<T>)) };
        unsafe { utp_set_callback(ctx, UTP_ON_ERROR as c_int, Some(utp_on_error::<T>)) };
        unsafe { utp_set_callback(ctx, UTP_ON_ACCEPT as c_int, Some(utp_on_accept::<T>)) };

        let res = Arc::new(Self {
            transport,
            ctx,
            accept_queue: Default::default(),
        });
        let resptr: *const Self = res.as_ref();
        unsafe { utp_context_set_userdata(ctx, resptr.cast_mut().cast()) };

        Self::spawn(res.clone());
        Some(res)
    }

    fn recv_from<'a>(
        &'a self,
        buf: &'a mut [u8],
    ) -> impl Future<Output = std::io::Result<(usize, SocketAddr)>> + 'a {
        std::future::poll_fn(|cx| match self.transport.poll_recv_from(cx, buf) {
            Poll::Ready(res) => Poll::Ready(res),
            Poll::Pending => {
                with_global_lock(|| unsafe {
                    utp_issue_deferred_acks(self.ctx);
                });
                Poll::Pending
            }
        })
    }

    pub fn spawn(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut buf = [0u8; 16384];
            let mut timeout_interval = tokio::time::interval(Duration::from_millis(500));
            loop {
                tokio::select! {
                    res = self.recv_from(&mut buf) => {
                        let (len, addr) = res.unwrap();
                        let osaddr = os_socketaddr::OsSocketAddr::from(addr);
                        unsafe {
                            let res = with_global_lock(|| {
                                utp_process_udp(
                                    self.ctx,
                                    &buf as *const u8,
                                    len,
                                    osaddr.as_ptr().cast(),
                                    osaddr.len(),
                                )
                            });
                            if res < 0 {
                                warn!(res, "utp_process_udp errored");
                            }
                        };
                    },
                    _ = timeout_interval.tick() => {
                        unsafe {
                            with_global_lock(|| {
                                // info!("utp_check_timeouts");
                                utp_check_timeouts(self.ctx);
                            })
                        }
                    }
                };
            }
        });
    }

    pub async fn accept(&self) -> Option<UtpStream<T>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.accept_queue.lock().push_back(tx);

        let stream = rx.await.unwrap();
        Some(stream)
    }

    pub async fn connect(self: &Arc<Self>, addr: SocketAddr) -> anyhow::Result<UtpStream<T>> {
        with_global_lock(|| {
            let sock = unsafe { utp_create_socket(self.ctx) };
            if sock.is_null() {
                bail!("utp_create_socket returned null");
            }

            let stream = UtpStream::<T>::new(sock, self.clone());
            let addr = os_socketaddr::OsSocketAddr::from(addr);

            let ret = unsafe { utp_connect(sock, addr.as_ptr().cast(), addr.len()) };
            if ret < 0 {
                bail!("utp_connect returned an error");
            }
            Ok(stream)
        })
    }
}

impl<T> AsyncRead for UtpStream<T> {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let mut g = self.inner.buffer.lock();
        let len = g.pop_slice(buf.initialize_unfilled());
        buf.advance(len);
        if len > 0 {
            return Poll::Ready(Ok(()));
        }
        if self.inner.is_eof.load(std::sync::atomic::Ordering::SeqCst) {
            return Poll::Ready(Ok(()));
        }
        self.inner.readable_waker.register(cx.waker());
        Poll::Pending
    }
}

impl<T: Transport> AsyncWrite for UtpStream<T> {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        with_global_lock(|| {
            let ctx = self.get_context();
            match ctx.transport.poll_send_ready(cx) {
                Poll::Ready(Ok(())) => {}
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            };

            let len = unsafe {
                utp_write(
                    self.inner.utp_socket,
                    buf.as_ptr().cast_mut().cast(),
                    buf.len(),
                )
            };
            // trace!(expected = buf.len(), len, "utp_write");
            if len == 0 {
                self.inner.writeable_waker.register(cx.waker());
                return Poll::Pending;
            }
            Poll::Ready(Ok(len as usize))
        })
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        todo!()
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        todo!()
    }
}

pub type UtpUdpContext = UtpContext<tokio::net::UdpSocket>;
pub type UtpUdpStream = UtpStream<tokio::net::UdpSocket>;
