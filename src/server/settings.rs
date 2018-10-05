use std::cell::{RefCell, RefMut, UnsafeCell};
use std::collections::VecDeque;
use std::fmt::Write;
use std::rc::Rc;
use std::time::{Duration, Instant};
use std::{env, fmt, net};

use bytes::BytesMut;
use futures::{future, Future};
use http::StatusCode;
use time;
use tokio_current_thread::spawn;
use tokio_timer::{sleep, Delay};

use super::message::{Request, RequestPool};
use super::KeepAlive;
use body::Body;
use httpresponse::{HttpResponse, HttpResponseBuilder, HttpResponsePool};

// "Sun, 06 Nov 1994 08:49:37 GMT".len()
const DATE_VALUE_LENGTH: usize = 29;

/// Http service configuration
pub struct ServiceConfig(Rc<Inner>);

struct Inner {
    keep_alive: Option<Duration>,
    client_timeout: u64,
    client_shutdown: u64,
    ka_enabled: bool,
    bytes: Rc<SharedBytesPool>,
    messages: &'static RequestPool,
    date: UnsafeCell<(bool, Date)>,
}

impl Clone for ServiceConfig {
    fn clone(&self) -> Self {
        ServiceConfig(self.0.clone())
    }
}

impl ServiceConfig {
    /// Create instance of `ServiceConfig`
    pub(crate) fn new(
        keep_alive: KeepAlive, client_timeout: u64, client_shutdown: u64,
    ) -> ServiceConfig {
        let (keep_alive, ka_enabled) = match keep_alive {
            KeepAlive::Timeout(val) => (val as u64, true),
            KeepAlive::Os => (0, true),
            KeepAlive::Disabled => (0, false),
        };
        let keep_alive = if ka_enabled && keep_alive > 0 {
            Some(Duration::from_secs(keep_alive))
        } else {
            None
        };

        ServiceConfig(Rc::new(Inner {
            keep_alive,
            ka_enabled,
            client_timeout,
            client_shutdown,
            bytes: Rc::new(SharedBytesPool::new()),
            messages: RequestPool::pool(),
            date: UnsafeCell::new((false, Date::new())),
        }))
    }

    /// Create worker settings builder.
    pub fn build() -> ServiceConfigBuilder {
        ServiceConfigBuilder::new()
    }

    #[inline]
    /// Keep alive duration if configured.
    pub fn keep_alive(&self) -> Option<Duration> {
        self.0.keep_alive
    }

    #[inline]
    /// Return state of connection keep-alive funcitonality
    pub fn keep_alive_enabled(&self) -> bool {
        self.0.ka_enabled
    }

    pub(crate) fn get_bytes(&self) -> BytesMut {
        self.0.bytes.get_bytes()
    }

    pub(crate) fn release_bytes(&self, bytes: BytesMut) {
        self.0.bytes.release_bytes(bytes)
    }

    pub(crate) fn get_request(&self) -> Request {
        RequestPool::get(self.0.messages)
    }

    #[doc(hidden)]
    pub fn request_pool(&self) -> &'static RequestPool {
        self.0.messages
    }

    fn update_date(&self) {
        // Unsafe: WorkerSetting is !Sync and !Send
        unsafe { (*self.0.date.get()).0 = false };
    }

    #[inline]
    /// Client timeout for first request.
    pub fn client_timer(&self) -> Option<Delay> {
        let delay = self.0.client_timeout;
        if delay != 0 {
            Some(Delay::new(self.now() + Duration::from_millis(delay)))
        } else {
            None
        }
    }

    /// Client timeout for first request.
    pub fn client_timer_expire(&self) -> Option<Instant> {
        let delay = self.0.client_timeout;
        if delay != 0 {
            Some(self.now() + Duration::from_millis(delay))
        } else {
            None
        }
    }

    /// Client shutdown timer
    pub fn client_shutdown_timer(&self) -> Option<Instant> {
        let delay = self.0.client_shutdown;
        if delay != 0 {
            Some(self.now() + Duration::from_millis(delay))
        } else {
            None
        }
    }

    #[inline]
    /// Return keep-alive timer delay is configured.
    pub fn keep_alive_timer(&self) -> Option<Delay> {
        if let Some(ka) = self.0.keep_alive {
            Some(Delay::new(self.now() + ka))
        } else {
            None
        }
    }

    /// Keep-alive expire time
    pub fn keep_alive_expire(&self) -> Option<Instant> {
        if let Some(ka) = self.0.keep_alive {
            Some(self.now() + ka)
        } else {
            None
        }
    }

    pub(crate) fn set_date(&self, dst: &mut BytesMut, full: bool) {
        // Unsafe: WorkerSetting is !Sync and !Send
        let date_bytes = unsafe {
            let date = &mut (*self.0.date.get());
            if !date.0 {
                date.1.update();
                date.0 = true;

                // periodic date update
                let s = self.clone();
                spawn(sleep(Duration::from_millis(500)).then(move |_| {
                    s.update_date();
                    future::ok(())
                }));
            }
            &date.1.bytes
        };
        if full {
            let mut buf: [u8; 39] = [0; 39];
            buf[..6].copy_from_slice(b"date: ");
            buf[6..35].copy_from_slice(date_bytes);
            buf[35..].copy_from_slice(b"\r\n\r\n");
            dst.extend_from_slice(&buf);
        } else {
            dst.extend_from_slice(date_bytes);
        }
    }

    #[inline]
    pub(crate) fn now(&self) -> Instant {
        unsafe {
            let date = &mut (*self.0.date.get());
            if !date.0 {
                date.1.update();
                date.0 = true;

                // periodic date update
                let s = self.clone();
                spawn(sleep(Duration::from_millis(500)).then(move |_| {
                    s.update_date();
                    future::ok(())
                }));
            }
            date.1.current
        }
    }
}

/// A service config builder
///
/// This type can be used to construct an instance of `ServiceConfig` through a
/// builder-like pattern.
pub struct ServiceConfigBuilder {
    keep_alive: KeepAlive,
    client_timeout: u64,
    client_shutdown: u64,
    host: String,
    addr: net::SocketAddr,
    secure: bool,
}

impl ServiceConfigBuilder {
    /// Create instance of `ServiceConfigBuilder`
    pub fn new() -> ServiceConfigBuilder {
        ServiceConfigBuilder {
            keep_alive: KeepAlive::Timeout(5),
            client_timeout: 5000,
            client_shutdown: 5000,
            secure: false,
            host: "localhost".to_owned(),
            addr: "127.0.0.1:8080".parse().unwrap(),
        }
    }

    /// Enable secure flag for current server.
    ///
    /// By default this flag is set to false.
    pub fn secure(mut self) -> Self {
        self.secure = true;
        self
    }

    /// Set server keep-alive setting.
    ///
    /// By default keep alive is set to a 5 seconds.
    pub fn keep_alive<T: Into<KeepAlive>>(mut self, val: T) -> Self {
        self.keep_alive = val.into();
        self
    }

    /// Set server client timeout in milliseconds for first request.
    ///
    /// Defines a timeout for reading client request header. If a client does not transmit
    /// the entire set headers within this time, the request is terminated with
    /// the 408 (Request Time-out) error.
    ///
    /// To disable timeout set value to 0.
    ///
    /// By default client timeout is set to 5000 milliseconds.
    pub fn client_timeout(mut self, val: u64) -> Self {
        self.client_timeout = val;
        self
    }

    /// Set server connection shutdown timeout in milliseconds.
    ///
    /// Defines a timeout for shutdown connection. If a shutdown procedure does not complete
    /// within this time, the request is dropped. This timeout affects only secure connections.
    ///
    /// To disable timeout set value to 0.
    ///
    /// By default client timeout is set to 5000 milliseconds.
    pub fn client_shutdown(mut self, val: u64) -> Self {
        self.client_shutdown = val;
        self
    }

    /// Set server host name.
    ///
    /// Host name is used by application router aa a hostname for url
    /// generation. Check [ConnectionInfo](./dev/struct.ConnectionInfo.
    /// html#method.host) documentation for more information.
    ///
    /// By default host name is set to a "localhost" value.
    pub fn server_hostname(mut self, val: &str) -> Self {
        self.host = val.to_owned();
        self
    }

    /// Set server ip address.
    ///
    /// Host name is used by application router aa a hostname for url
    /// generation. Check [ConnectionInfo](./dev/struct.ConnectionInfo.
    /// html#method.host) documentation for more information.
    ///
    /// By default server address is set to a "127.0.0.1:8080"
    pub fn server_address<S: net::ToSocketAddrs>(mut self, addr: S) -> Self {
        match addr.to_socket_addrs() {
            Err(err) => error!("Can not convert to SocketAddr: {}", err),
            Ok(mut addrs) => if let Some(addr) = addrs.next() {
                self.addr = addr;
            },
        }
        self
    }

    /// Finish service configuration and create `ServiceConfig` object.
    pub fn finish(self) -> ServiceConfig {
        let client_shutdown = if self.secure { self.client_shutdown } else { 0 };

        ServiceConfig::new(self.keep_alive, self.client_timeout, client_shutdown)
    }
}

struct Date {
    current: Instant,
    bytes: [u8; DATE_VALUE_LENGTH],
    pos: usize,
}

impl Date {
    fn new() -> Date {
        let mut date = Date {
            current: Instant::now(),
            bytes: [0; DATE_VALUE_LENGTH],
            pos: 0,
        };
        date.update();
        date
    }
    fn update(&mut self) {
        self.pos = 0;
        self.current = Instant::now();
        write!(self, "{}", time::at_utc(time::get_time()).rfc822()).unwrap();
    }
}

impl fmt::Write for Date {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let len = s.len();
        self.bytes[self.pos..self.pos + len].copy_from_slice(s.as_bytes());
        self.pos += len;
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct SharedBytesPool(RefCell<VecDeque<BytesMut>>);

impl SharedBytesPool {
    pub fn new() -> SharedBytesPool {
        SharedBytesPool(RefCell::new(VecDeque::with_capacity(128)))
    }

    pub fn get_bytes(&self) -> BytesMut {
        if let Some(bytes) = self.0.borrow_mut().pop_front() {
            bytes
        } else {
            BytesMut::new()
        }
    }

    pub fn release_bytes(&self, mut bytes: BytesMut) {
        let v = &mut self.0.borrow_mut();
        if v.len() < 128 {
            bytes.clear();
            v.push_front(bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future;
    use tokio::runtime::current_thread;

    #[test]
    fn test_date_len() {
        assert_eq!(DATE_VALUE_LENGTH, "Sun, 06 Nov 1994 08:49:37 GMT".len());
    }

    #[test]
    fn test_date() {
        let mut rt = current_thread::Runtime::new().unwrap();

        let _ = rt.block_on(future::lazy(|| {
            let settings = ServiceConfig::<()>::new(
                (),
                KeepAlive::Os,
                0,
                0,
                ServerSettings::default(),
            );
            let mut buf1 = BytesMut::with_capacity(DATE_VALUE_LENGTH + 10);
            settings.set_date(&mut buf1, true);
            let mut buf2 = BytesMut::with_capacity(DATE_VALUE_LENGTH + 10);
            settings.set_date(&mut buf2, true);
            assert_eq!(buf1, buf2);
            future::ok::<_, ()>(())
        }));
    }
}
