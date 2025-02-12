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
    utp_context_get_userdata, utp_context_set_option, utp_context_set_userdata, utp_create_socket,
    utp_destroy, utp_error_code_names, utp_get_userdata, utp_init, utp_issue_deferred_acks,
    utp_process_udp, utp_read_drained, utp_set_callback, utp_set_userdata, utp_socket,
    utp_state_names, utp_write, UTP_LOG, UTP_LOG_DEBUG, UTP_LOG_NORMAL, UTP_ON_ACCEPT,
    UTP_ON_CONNECT, UTP_ON_ERROR, UTP_ON_READ, UTP_ON_STATE_CHANGE, UTP_SENDTO, UTP_STATE_CONNECT,
    UTP_STATE_DESTROYING, UTP_STATE_EOF, UTP_STATE_WRITABLE,
};
use os_socketaddr::OsSocketAddr;
use parking_lot::{Mutex, ReentrantMutex};
use ringbuf::{
    storage::Heap,
    traits::{Consumer, Producer},
    LocalRb,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::{debug, trace, warn};
pub use traits::Transport;

static LOCK: ReentrantMutex<()> = ReentrantMutex::new(());

const MAGIC: usize = 0x42424242_42424242;

pub fn with_global_lock<R>(f: impl FnOnce() -> R) -> R {
    let _g = LOCK.lock();
    f()
}

unsafe extern "C" fn utp_on_connect<T>(args: *mut utp_callback_arguments) -> uint64 {
    let args = args.as_mut().unwrap();
    let data: *const UtpStreamInner<T> = utp_get_userdata(args.socket).cast();
    if data.is_null() {
        warn!("utp_on_connect: null socket");
        return 0;
    }
    let data = data.as_ref().unwrap();
    assert!(data.magic == MAGIC);
    data.writeable_waker.wake();
    0
}

#[allow(unused)]
unsafe extern "C" fn utp_log(args: *mut utp_callback_arguments) -> uint64 {
    let args = args.as_mut().unwrap();
    let logline = CStr::from_ptr(args.buf.cast()).to_str().unwrap();
    trace!("{}", logline);
    0
}

unsafe extern "C" fn utp_on_read<T>(args: *mut utp_callback_arguments) -> uint64 {
    let args = args.as_mut().unwrap();
    let data = utp_get_userdata(args.socket) as SocketUserData<T>;
    if data.is_null() {
        warn!("utp_on_read called with null socket");
        return 0;
    }
    let data = data.as_ref().unwrap();
    assert!(data.magic == MAGIC);

    let inbuf = std::slice::from_raw_parts(args.buf, args.len);
    let mut buf = data.buffer.lock();
    buf.push_slice(inbuf);
    utp_read_drained(args.socket);
    data.readable_waker.wake();
    0
}

unsafe extern "C" fn utp_on_sendto<T: Transport>(args: *mut utp_callback_arguments) -> uint64 {
    let args = args.as_mut().unwrap();
    let addr = args.unnamed_field1.address;
    let addr = OsSocketAddr::copy_from_raw(addr.cast(), args.unnamed_field2.address_len)
        .into_addr()
        .unwrap();
    let udata: *const UtpContext<T> = utp_context_get_userdata(args.context).cast();
    if udata.is_null() {
        warn!("utp_on_sendto: null context");
        return 0;
    }
    let udata = udata.as_ref().unwrap();
    assert!(udata.magic == MAGIC);
    let buf = core::slice::from_raw_parts(args.buf, args.len);
    let sz = udata.transport.try_send_to(buf, addr);
    match sz {
        Ok(0) => warn!("sent 0"),
        Ok(_) => {}
        Err(e) => warn!("send error: {e:#}"),
    }

    0
}

unsafe extern "C" fn utp_on_error(args: *mut utp_callback_arguments) -> uint64 {
    let args = args.as_mut().unwrap();
    let error = args.unnamed_field1.error_code;
    #[allow(static_mut_refs)]
    let error = get_name(utp_error_code_names.as_ptr(), error);
    debug!("utp_on_error: {error:?}");
    0
}

unsafe fn get_name(arr: *const *const c_char, name: i32) -> &'static str {
    let cptr = *arr.offset(name as isize);
    CStr::from_ptr(cptr).to_str().unwrap()
}

unsafe extern "C" fn utp_on_accept<T: Transport>(args: *mut utp_callback_arguments) -> uint64 {
    let args = args.as_mut().unwrap();

    let ctx: *const UtpContext<T> = utp_context_get_userdata(args.context).cast();
    if ctx.is_null() {
        debug!("utp_on_accept: null ctx");
        return 0;
    }
    let ctx = Arc::from_raw(ctx);
    assert!(ctx.magic == MAGIC);
    if let Some(acc) = ctx.accept_queue.lock().pop_front() {
        let stream = UtpStream::new(args.socket, ctx.clone());
        let _ = acc.send(stream);
    }
    std::mem::forget(ctx);
    0
}

unsafe extern "C" fn utp_on_state_change<T>(args: *mut utp_callback_arguments) -> uint64 {
    let args = args.as_mut().unwrap();
    let data: *const UtpStreamInner<T> = utp_get_userdata(args.socket).cast();
    if data.is_null() {
        debug!("utp_on_state_change: null userdata");
        return 0;
    }
    let data = data.as_ref().unwrap();
    assert!(data.magic == MAGIC);

    let state = args.unnamed_field1.state;
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
        other => {
            warn!(other, "unknown libutp state")
        }
    };
    0
}

type SocketUserData<T> = *const UtpStreamInner<T>;

pub struct UtpContext<T> {
    transport: T,

    magic: usize,

    accept_queue: Mutex<VecDeque<tokio::sync::oneshot::Sender<UtpStream<T>>>>,

    // TODO: store hidden pointer that can only be looked at with the global lock
    ctx: *mut libutp_rs2_sys::utp_context,
}

impl<T> Drop for UtpContext<T> {
    fn drop(&mut self) {
        unsafe {
            with_global_lock(|| {
                utp_context_set_userdata(self.ctx, core::ptr::null_mut());
                utp_destroy(self.ctx);
            })
        }
    }
}

unsafe impl<T: Send> Send for UtpContext<T> {}
unsafe impl<T: Send> Sync for UtpContext<T> {}

struct UtpStreamInner<T> {
    buffer: Mutex<LocalRb<Heap<u8>>>,
    magic: usize,
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
            magic: MAGIC,
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
            utp_set_userdata(self.inner.utp_socket, core::ptr::null_mut());
            utp_close(self.inner.utp_socket);
        }
    }
}

impl UtpUdpContext {
    pub async fn new_udp(bind_addr: SocketAddr) -> anyhow::Result<Arc<Self>> {
        Self::new_udp_with_opts(bind_addr, Default::default()).await
    }

    pub async fn new_udp_with_opts(
        bind_addr: SocketAddr,
        opts: UtpOpts,
    ) -> anyhow::Result<Arc<Self>> {
        let sock = tokio::net::UdpSocket::bind(bind_addr).await.unwrap();
        Self::new(sock, opts)
    }
}

#[derive(Default)]
pub enum UtpLogLevel {
    #[default]
    None,
    Normal,
    Debug,
}

#[derive(Default)]
pub struct UtpOpts {
    pub log_level: UtpLogLevel,
}

impl<T: Transport> UtpContext<T> {
    pub fn new(transport: T, opts: UtpOpts) -> anyhow::Result<Arc<Self>> {
        unsafe {
            let ctx = utp_init(2);
            if ctx.is_null() {
                bail!("utp_init returned NULL");
            }

            match opts.log_level {
                UtpLogLevel::None => {}
                UtpLogLevel::Normal => {
                    utp_context_set_option(ctx, UTP_LOG_NORMAL as _, 1);
                }
                UtpLogLevel::Debug => {
                    utp_context_set_option(ctx, UTP_LOG_NORMAL as _, 1);
                    utp_context_set_option(ctx, UTP_LOG_DEBUG as _, 1);
                }
            };

            utp_set_callback(ctx, UTP_ON_CONNECT as c_int, Some(utp_on_connect::<T>));
            utp_set_callback(ctx, UTP_LOG as c_int, Some(utp_log));
            utp_set_callback(
                ctx,
                UTP_ON_STATE_CHANGE as c_int,
                Some(utp_on_state_change::<T>),
            );
            utp_set_callback(ctx, UTP_ON_READ as c_int, Some(utp_on_read::<T>));
            utp_set_callback(ctx, UTP_SENDTO as c_int, Some(utp_on_sendto::<T>));
            utp_set_callback(ctx, UTP_ON_ERROR as c_int, Some(utp_on_error));
            utp_set_callback(ctx, UTP_ON_ACCEPT as c_int, Some(utp_on_accept::<T>));

            let res = Arc::new(Self {
                transport,
                ctx,
                accept_queue: Default::default(),
                magic: MAGIC,
            });
            utp_context_set_userdata(ctx, Arc::as_ptr(&res).cast_mut().cast());

            Self::spawn(res.clone());
            Ok(res)
        }
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
                        let osaddr = OsSocketAddr::from(addr);
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
                                debug!(res, "utp_process_udp errored");
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
            let addr = OsSocketAddr::from(addr);

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
