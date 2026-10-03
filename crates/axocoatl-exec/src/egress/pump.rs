//! Copy bytes both ways between two connected sockets until both sides close.
//!
//! One thread per connection, `poll(2)` on both descriptors and a bounded
//! 64 KiB buffer in each direction. A half-close is passed on once the
//! buffered bytes are delivered. The pump stops on an idle timeout or when
//! the connection is revoked (its sockets are shut down from outside).

use std::io;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::protocol::CloseOutcome;

/// Buffer size in each direction.
pub const PUMP_BUFFER_BYTES: usize = 64 * 1024;

/// What a finished pump reports for the close frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PumpReport {
    /// Bytes written to the upstream side.
    pub up: u64,
    /// Bytes written to the client side.
    pub down: u64,
    pub outcome: CloseOutcome,
    pub error: Option<String>,
}

struct Lane {
    buffer: Vec<u8>,
    start: usize,
    end: usize,
}

impl Lane {
    fn new(initial: &[u8]) -> Self {
        let mut buffer = vec![0; PUMP_BUFFER_BYTES.max(initial.len())];
        buffer[..initial.len()].copy_from_slice(initial);
        Self {
            buffer,
            start: 0,
            end: initial.len(),
        }
    }

    fn pending(&self) -> bool {
        self.start < self.end
    }

    fn has_room(&self) -> bool {
        self.end < self.buffer.len() || self.start > 0
    }

    fn compact(&mut self) {
        if self.start == self.end {
            self.start = 0;
            self.end = 0;
        } else if self.end == self.buffer.len() && self.start > 0 {
            self.buffer.copy_within(self.start..self.end, 0);
            self.end -= self.start;
            self.start = 0;
        }
    }
}

pub(crate) fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: fcntl on a descriptor the caller owns, with no pointer arguments.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Never raise SIGPIPE for writes on this socket.
pub(crate) fn no_sigpipe(fd: RawFd) {
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
    {
        let one: libc::c_int = 1;
        // SAFETY: setsockopt with a valid pointer to a c_int of the stated size.
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_NOSIGPIPE,
                (&one as *const libc::c_int).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "freebsd")))]
    let _ = fd;
}

#[cfg(any(target_os = "linux", target_os = "android"))]
const SEND_FLAGS: libc::c_int = libc::MSG_NOSIGNAL;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
const SEND_FLAGS: libc::c_int = 0;

/// Shut down one or both directions of a socket. Errors are ignored: the
/// peer may already be gone.
pub(crate) fn shutdown(fd: RawFd, how: libc::c_int) {
    // SAFETY: shutdown on a socket descriptor; no memory is passed.
    unsafe {
        libc::shutdown(fd, how);
    }
}

enum Io {
    Moved(usize),
    Eof,
    WouldBlock,
    Failed(io::Error),
}

fn read_into(fd: RawFd, lane: &mut Lane) -> Io {
    lane.compact();
    let room = &mut lane.buffer[lane.end..];
    if room.is_empty() {
        return Io::WouldBlock;
    }
    // SAFETY: the pointer and length describe the writable tail of the buffer.
    let read = unsafe { libc::read(fd, room.as_mut_ptr().cast(), room.len()) };
    match read {
        0 => Io::Eof,
        n if n > 0 => {
            lane.end += n as usize;
            Io::Moved(n as usize)
        }
        _ => classify(io::Error::last_os_error()),
    }
}

/// One nonblocking `send` of `bytes` without raising SIGPIPE.
pub(crate) fn send_some(fd: RawFd, bytes: &[u8]) -> io::Result<usize> {
    if bytes.is_empty() {
        return Ok(0);
    }
    loop {
        // SAFETY: the pointer and length describe initialized bytes.
        let written = unsafe { libc::send(fd, bytes.as_ptr().cast(), bytes.len(), SEND_FLAGS) };
        if written >= 0 {
            return Ok(written as usize);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn write_from(fd: RawFd, lane: &mut Lane) -> Io {
    let pending = &lane.buffer[lane.start..lane.end];
    if pending.is_empty() {
        return Io::WouldBlock;
    }
    // SAFETY: the pointer and length describe initialized pending bytes.
    let written = unsafe { libc::send(fd, pending.as_ptr().cast(), pending.len(), SEND_FLAGS) };
    if written >= 0 {
        lane.start += written as usize;
        lane.compact();
        Io::Moved(written as usize)
    } else {
        classify(io::Error::last_os_error())
    }
}

fn classify(error: io::Error) -> Io {
    match error.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => Io::WouldBlock,
        _ => Io::Failed(error),
    }
}

/// Copy between `client` and `upstream`, first sending `initial_up` upstream.
/// Both descriptors are made nonblocking. Returns when both directions have
/// closed, on an error, on `idle` without traffic, or when `revoked` is set.
pub fn pump(
    client: RawFd,
    upstream: RawFd,
    initial_up: &[u8],
    idle: Duration,
    revoked: &AtomicBool,
) -> PumpReport {
    let mut report = PumpReport {
        up: 0,
        down: 0,
        outcome: CloseOutcome::Closed,
        error: None,
    };
    for fd in [client, upstream] {
        no_sigpipe(fd);
        if let Err(error) = set_nonblocking(fd) {
            report.outcome = CloseOutcome::Reset;
            report.error = Some(error.to_string());
            return report;
        }
    }
    let mut to_upstream = Lane::new(initial_up);
    let mut to_client = Lane::new(&[]);
    let (mut client_eof, mut upstream_eof) = (false, false);
    let (mut upstream_shut, mut client_shut) = (false, false);
    let mut last_activity = Instant::now();
    let fail = |report: &mut PumpReport, error: io::Error, revoked: &AtomicBool| {
        report.outcome = if revoked.load(Ordering::Acquire) {
            CloseOutcome::Revoked
        } else {
            CloseOutcome::Reset
        };
        report.error = Some(error.to_string());
    };
    loop {
        if revoked.load(Ordering::Acquire) {
            report.outcome = CloseOutcome::Revoked;
            return report;
        }
        if client_eof && !to_upstream.pending() && !upstream_shut {
            shutdown(upstream, libc::SHUT_WR);
            upstream_shut = true;
        }
        if upstream_eof && !to_client.pending() && !client_shut {
            shutdown(client, libc::SHUT_WR);
            client_shut = true;
        }
        if upstream_shut && client_shut {
            return report;
        }
        let interest = |reading: bool, writing: bool| {
            (if reading { libc::POLLIN } else { 0 }) | (if writing { libc::POLLOUT } else { 0 })
        };
        let client_events = interest(!client_eof && to_upstream.has_room(), to_client.pending());
        let upstream_events =
            interest(!upstream_eof && to_client.has_room(), to_upstream.pending());
        let mut fds = [
            libc::pollfd {
                fd: if client_events == 0 { -1 } else { client },
                events: client_events,
                revents: 0,
            },
            libc::pollfd {
                fd: if upstream_events == 0 { -1 } else { upstream },
                events: upstream_events,
                revents: 0,
            },
        ];
        let remaining = idle.saturating_sub(last_activity.elapsed());
        if remaining.is_zero() {
            report.outcome = CloseOutcome::IdleTimeout;
            return report;
        }
        let wait = remaining.min(Duration::from_millis(500)).as_millis() as libc::c_int;
        // SAFETY: fds is a live array of two pollfd values.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, wait.max(1)) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            fail(&mut report, error, revoked);
            return report;
        }
        if ready == 0 {
            continue;
        }
        let readable =
            |revents: libc::c_short| revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0;
        let writable =
            |revents: libc::c_short| revents & (libc::POLLOUT | libc::POLLHUP | libc::POLLERR) != 0;
        if fds[0].fd >= 0 && client_events & libc::POLLIN != 0 && readable(fds[0].revents) {
            match read_into(client, &mut to_upstream) {
                Io::Moved(_) => last_activity = Instant::now(),
                Io::Eof => client_eof = true,
                Io::WouldBlock => {}
                Io::Failed(error) => {
                    fail(&mut report, error, revoked);
                    return report;
                }
            }
        }
        if fds[1].fd >= 0 && upstream_events & libc::POLLIN != 0 && readable(fds[1].revents) {
            match read_into(upstream, &mut to_client) {
                Io::Moved(_) => last_activity = Instant::now(),
                Io::Eof => upstream_eof = true,
                Io::WouldBlock => {}
                Io::Failed(error) => {
                    fail(&mut report, error, revoked);
                    return report;
                }
            }
        }
        if fds[0].fd >= 0 && client_events & libc::POLLOUT != 0 && writable(fds[0].revents) {
            match write_from(client, &mut to_client) {
                Io::Moved(count) => {
                    report.down += count as u64;
                    last_activity = Instant::now();
                }
                Io::Eof | Io::WouldBlock => {}
                Io::Failed(error) => {
                    fail(&mut report, error, revoked);
                    return report;
                }
            }
        }
        if fds[1].fd >= 0 && upstream_events & libc::POLLOUT != 0 && writable(fds[1].revents) {
            match write_from(upstream, &mut to_upstream) {
                Io::Moved(count) => {
                    report.up += count as u64;
                    last_activity = Instant::now();
                }
                Io::Eof | Io::WouldBlock => {}
                Io::Failed(error) => {
                    fail(&mut report, error, revoked);
                    return report;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;

    fn run_pump(
        initial: Vec<u8>,
        idle: Duration,
    ) -> (
        UnixStream,
        UnixStream,
        std::sync::Arc<AtomicBool>,
        std::thread::JoinHandle<PumpReport>,
    ) {
        let (client_outer, client_inner) = UnixStream::pair().unwrap();
        let (upstream_inner, upstream_outer) = UnixStream::pair().unwrap();
        let revoked = std::sync::Arc::new(AtomicBool::new(false));
        let flag = revoked.clone();
        let handle = std::thread::spawn(move || {
            let report = pump(
                client_inner.as_raw_fd(),
                upstream_inner.as_raw_fd(),
                &initial,
                idle,
                &flag,
            );
            drop(client_inner);
            drop(upstream_inner);
            report
        });
        (client_outer, upstream_outer, revoked, handle)
    }

    #[test]
    fn copies_a_mebibyte_each_way_and_passes_half_close() {
        let (mut client, mut upstream, _revoked, handle) =
            run_pump(b"HEAD".to_vec(), Duration::from_secs(10));
        let payload: Vec<u8> = (0..1024 * 1024).map(|index| (index % 251) as u8).collect();
        let upload = payload.clone();
        let mut client_writer = client.try_clone().unwrap();
        let writer = std::thread::spawn(move || {
            client_writer.write_all(&upload).unwrap();
            client_writer.shutdown(std::net::Shutdown::Write).unwrap();
        });
        let mut upstream_writer = upstream.try_clone().unwrap();
        let download = payload.clone();
        let responder = std::thread::spawn(move || {
            upstream_writer.write_all(&download).unwrap();
            upstream_writer.shutdown(std::net::Shutdown::Write).unwrap();
        });
        let mut received_up = Vec::new();
        upstream.read_to_end(&mut received_up).unwrap();
        let mut received_down = Vec::new();
        client.read_to_end(&mut received_down).unwrap();
        writer.join().unwrap();
        responder.join().unwrap();
        assert_eq!(&received_up[..4], b"HEAD");
        assert_eq!(&received_up[4..], &payload[..]);
        assert_eq!(received_down, payload);
        let report = handle.join().unwrap();
        assert_eq!(report.outcome, CloseOutcome::Closed);
        assert_eq!(report.up, payload.len() as u64 + 4);
        assert_eq!(report.down, payload.len() as u64);
    }

    #[test]
    fn idle_connections_time_out() {
        let (_client, _upstream, _revoked, handle) =
            run_pump(Vec::new(), Duration::from_millis(150));
        let report = handle.join().unwrap();
        assert_eq!(report.outcome, CloseOutcome::IdleTimeout);
    }

    #[test]
    fn revoking_stops_the_pump() {
        let (client, _upstream, revoked, handle) = run_pump(Vec::new(), Duration::from_secs(30));
        std::thread::sleep(Duration::from_millis(50));
        revoked.store(true, Ordering::Release);
        // Shutting the client down wakes the poll, as the proxy does.
        client.shutdown(std::net::Shutdown::Both).unwrap();
        let report = handle.join().unwrap();
        assert_eq!(report.outcome, CloseOutcome::Revoked);
    }

    #[test]
    fn an_abrupt_peer_ends_the_pump() {
        let (client, upstream, _revoked, handle) = run_pump(Vec::new(), Duration::from_secs(30));
        drop(upstream);
        drop(client);
        let report = handle.join().unwrap();
        assert!(
            matches!(report.outcome, CloseOutcome::Closed | CloseOutcome::Reset),
            "{report:?}"
        );
    }
}
