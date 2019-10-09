use std::fmt::Debug;
use std::io::BufReader;
use std::io::Error;
use std::net::SocketAddr;
use std::net::TcpListener;
use std::net::ToSocketAddrs;
use std::ops::DerefMut;
use std::os::unix::io::AsRawFd;
use std::os::unix::io::RawFd;
use std::sync::Arc;
use std::sync::Mutex;
use std::thread::spawn;
use std::thread::JoinHandle;

use bincode::deserialize_from;

use libc::close;
use libc::shutdown;
use libc::SHUT_RDWR;

use observe::Observable;
use observe::ObserverBox;
use observe::Subscription;
use observe::UpdatesSubscription;

use serde::de::DeserializeOwned;

use crate::message::Message;

#[derive(Copy, Clone, Debug)]
enum Fd {
    /// We are still listening for an incoming connection and
    /// this is the corresponding file descriptor.
    Listening(RawFd),
    /// We have accepted a connection and read data from it.
    Accepted(RawFd),
    /// The listener/accepted connection has been closed.
    Closed,
}

impl Fd {
    fn close(&mut self) -> Result<(), Error> {
        match *self {
            Fd::Listening(fd) | Fd::Accepted(fd) => {
                let rc = unsafe { shutdown(fd, SHUT_RDWR) };
                if rc != 0 {
                    return Err(Error::last_os_error());
                }

                // Bad luck if we fail the close. There is not much we
                // can do about that.
                *self = Fd::Closed;

                let rc = unsafe { close(fd) };
                if rc != 0 {
                    return Err(Error::last_os_error());
                }
                Ok(())
            }
            Fd::Closed => Ok(()),
        }
    }
}

/// The receiving end of a TCP channel has an address
/// and streams data to an observer.
#[derive(Debug)]
pub struct TcpReceiver<T> {
    /// The address we are listening on.
    addr: SocketAddr,
    /// Our listener/connection file descriptor state; shared with the
    /// thread accepting connections and reading streamed data.
    fd: Arc<Mutex<Fd>>,
    /// Handle to the thread accepting a connection and processing data.
    thread: Option<JoinHandle<Result<(), String>>>,
    /// The connected observer, if any.
    observer: Arc<Mutex<Option<ObserverBox<T, String>>>>,
}

impl<T> TcpReceiver<T>
where
    T: DeserializeOwned + Send + Debug + 'static,
{
    /// Create a new TCP receiver with no observer.
    ///
    /// `addr` may have a port set (by setting it to 0). In such a case
    /// the system will assign a port that is free. To retrieve this
    /// assigned port (in the form of the full `SocketAddr`), use the
    /// `addr` method.
    pub fn new<A>(addr: A) -> Result<Self, String>
    where
        A: ToSocketAddrs,
    {
        let listener =
            TcpListener::bind(addr).map_err(|e| format!("failed to bind TCP socket: {}", e))?;
        // We want to allow for auto-assigned ports, by letting the user
        // specify a `SocketAddr` with port 0. In this case, after
        // actually binding to an address, we need to update the port we
        // got assigned in `addr`, but for simplicity we just copy the
        // entire thing.
        let addr = listener
            .local_addr()
            .map_err(|e| format!("failed to inquire local address: {}", e))?;
        let fd = Arc::new(Mutex::new(Fd::Listening(listener.as_raw_fd())));
        let observer = Arc::new(Mutex::new(None));
        let thread = Some(Self::accept(listener, fd.clone(), observer.clone()));

        Ok(Self {
            addr,
            fd,
            thread,
            observer,
        })
    }

    /// Accept a connection (in a non-blocking manner), read data from
    /// it, and dispatch that to the subscribed observer, if any. If no
    /// observer is subscribed, data will be silently dropped.
    fn accept(
        listener: TcpListener,
        fd: Arc<Mutex<Fd>>,
        observer: Arc<Mutex<Option<ObserverBox<T, String>>>>,
    ) -> JoinHandle<Result<(), String>> {
        spawn(move || {
            let socket = match listener.accept() {
                Ok((s, _)) => {
                    let mut guard = fd.lock().unwrap();
                    // The user may have closed the receiver shortly
                    // after us accepting a connection. If that is the
                    // case do not continue.
                    if let Fd::Closed = *guard {
                        return Ok(());
                    }
                    *guard = Fd::Accepted(s.as_raw_fd());
                    s
                }
                Err(e) => {
                    // If the stream has been closed errors are expected
                    // and we just return to terminate the thread. We
                    // could alternatively check for a specific error
                    // return that occurs when the listener socket is
                    // closed concurrently but that seems less portable.
                    if let Fd::Closed = *fd.lock().unwrap() {
                        return Ok(());
                    } else {
                        return Err(format!("failed to accept connection: {}", e));
                    }
                }
            };

            let mut reader = BufReader::new(socket);
            loop {
                let message = match deserialize_from(&mut reader) {
                    Ok(m) => m,
                    Err(_) => {
                        if let Fd::Closed = *fd.lock().unwrap() {
                            return Ok(());
                        }
                        // TODO: Can/should we log the error?
                        continue;
                    }
                };

                // If there is no observer we just drop the data, which
                // is seemingly the only reasonable behavior given that
                // observers can come and go by virtue of our API
                // design.
                if let Some(ref mut observer) = observer.lock().unwrap().deref_mut() {
                    // TODO: Need to handle those errors eventually (or
                    //       perhaps we will end up with method
                    //       signatures that don't allow for errors?).
                    match message {
                        Message::Start => observer.on_start().unwrap(),
                        Message::Updates(updates) => {
                            observer.on_updates(Box::new(updates.into_iter())).unwrap()
                        }
                        Message::Commit => observer.on_commit().unwrap(),
                        Message::Complete => observer.on_completed().unwrap(),
                    }
                }
            }
        })
    }

    /// Retrieve the address we are listening on.
    pub fn addr(&self) -> &SocketAddr {
        &self.addr
    }
}

impl<T> Drop for TcpReceiver<T> {
    fn drop(&mut self) {
        // TODO: We probably want to just log failures.
        self.fd.lock().unwrap().close().unwrap();
        // TODO: We probably want to log any errors reported by the
        //       thread being joined.
        self.thread
            .take()
            .map(JoinHandle::join)
            .unwrap()
            .unwrap()
            .unwrap();

        // The remaining members will be destroyed automatically, no
        // need to bother here.
    }
}

impl<T> Observable<T, String> for TcpReceiver<T>
where
    T: Debug + Send + 'static,
{
    /// An observer subscribes to the receiving end of a TCP channel to
    /// listen to incoming data.
    fn subscribe(&mut self, observer: ObserverBox<T, String>) -> Option<Box<dyn Subscription>> {
        let mut guard = self.observer.lock().unwrap();
        match *guard {
            Some(_) => None,
            None => {
                *guard = Some(observer);
                Some(Box::new(UpdatesSubscription {
                    observer: self.observer.clone(),
                }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::ErrorKind;
    use std::net::TcpStream;

    /// Connect to a `TcpReceiver`.
    #[test]
    fn accept() {
        let recv = TcpReceiver::<()>::new("127.0.0.1:0").unwrap();
        let _ = TcpStream::connect(recv.addr()).unwrap();
    }

    /// Check that the listener socket is cleaned up properly when a
    /// `TcpReceiver` is dropped but has never accepted a connection.
    #[test]
    fn never_accepted() {
        let addr = {
            let recv = TcpReceiver::<()>::new("127.0.0.1:0").unwrap();
            recv.addr().clone()
        };

        let err = TcpStream::connect(addr).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::ConnectionRefused);
    }
}