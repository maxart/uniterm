//! Duplex SSH forwarding with one sleeping worker and a dedicated stop socket.
//! All data sockets are nonblocking so a stalled reader cannot delay detach.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

#[derive(Default)]
struct Chunk {
    bytes: Vec<u8>,
    start: usize,
    end: usize,
}

impl Chunk {
    fn empty(&self) -> bool {
        self.start == self.end
    }

    // Allocate at most one chunk per direction, on its first real read.
    fn read(&mut self, reader: &mut UnixStream) -> io::Result<bool> {
        if self.bytes.is_empty() {
            self.bytes.resize(16 * 1024, 0);
        }
        self.start = 0;
        self.end = 0;
        match reader.read(&mut self.bytes) {
            Ok(n) => {
                self.end = n;
                Ok(n != 0)
            }
            Err(e) if retry(&e) => Ok(true),
            Err(e) => Err(e),
        }
    }

    fn write(&mut self, writer: &mut UnixStream) -> io::Result<()> {
        self.start += write_some(writer, &self.bytes[self.start..self.end])?;
        Ok(())
    }
}

fn retry(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    )
}

fn write_some(writer: &mut UnixStream, data: &[u8]) -> io::Result<usize> {
    match writer.write(data) {
        Ok(0) => Err(io::ErrorKind::WriteZero.into()),
        Err(e) if retry(&e) => Ok(0),
        result => result,
    }
}

fn interest(socket: &UnixStream, read: bool, write: bool) -> libc::pollfd {
    let events = (if read { libc::POLLIN } else { 0 }) | (if write { libc::POLLOUT } else { 0 });
    libc::pollfd {
        // A disabled fd must not wake poll on a persistent hangup.
        fd: if events == 0 { -1 } else { socket.as_raw_fd() },
        events,
        revents: 0,
    }
}

fn ready(fd: &libc::pollfd, event: libc::c_short) -> bool {
    fd.revents & (event | libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0
}

pub(super) fn run(
    mut client: UnixStream,
    mut input: UnixStream,
    mut output: UnixStream,
    mut prefix: &[u8],
    stop: &UnixStream,
) -> io::Result<()> {
    for socket in [&client, &input, &output] {
        socket.set_nonblocking(true)?;
    }
    let mut upload = Chunk::default();
    let mut download = Chunk::default();
    let mut upload_closed = false;
    loop {
        let reading_client = !upload_closed && upload.empty();
        let mut fds = [
            interest(stop, true, false),
            interest(
                &client,
                reading_client,
                !prefix.is_empty() || !download.empty(),
            ),
            interest(&input, false, !upload.empty()),
            interest(&output, prefix.is_empty() && download.empty(), false),
        ];
        // SAFETY: each enabled fd belongs to a live socket for the entire wait.
        if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, -1) } < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if fds[0].revents != 0 {
            return Ok(());
        }
        let from_client = reading_client && ready(&fds[1], libc::POLLIN);
        if from_client && !upload.read(&mut client)? {
            upload_closed = true;
            let _ = input.shutdown(Shutdown::Write);
        }
        if !upload.empty() && (from_client || ready(&fds[2], libc::POLLOUT)) {
            if let Err(error) = upload.write(&mut input) {
                // SSH can close stdin and still have final output to deliver.
                if !super::disconnected(&error) {
                    return Err(error);
                }
                upload_closed = true;
                upload.start = upload.end;
            }
        }
        let from_remote = ready(&fds[3], libc::POLLIN);
        if from_remote && !download.read(&mut output)? {
            return Ok(());
        }
        if !prefix.is_empty() && ready(&fds[1], libc::POLLOUT) {
            prefix = &prefix[write_some(&mut client, prefix)?..];
        } else if !download.empty() && (from_remote || ready(&fds[1], libc::POLLOUT)) {
            download.write(&mut client)?;
        }
    }
}
