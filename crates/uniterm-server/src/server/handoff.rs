//! Private Workspace handoff transport. A parent creates the socket pair and
//! passes one end only to the verified candidate. Never accept this protocol
//! on an attach, control, manager, or inbox listener.

mod activation;
pub(crate) use activation::{capability, stage, Preparation};
pub use activation::{register_launcher, run_candidate};
mod layout;
mod model;
mod transfer;
pub use layout::WorkspaceLayout;
pub use model::WorkspaceModel;

use nix::sys::socket::{recvmsg, sendmsg, ControlMessage, ControlMessageOwned, MsgFlags};
use std::io::{self, IoSlice, IoSliceMut, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

// A fixed ancillary buffer bounds the number of new descriptors in one
// message. Large Workspaces transfer resources in bounded batches.
const MAX_RIGHTS: usize = 32;

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Private checkpoint format. Increment on incompatible engine or
/// terminal state changes, independently of the public attachment protocol.
pub const CHECKPOINT_VERSION: u32 = 3;
const MAGIC: [u8; 4] = *b"UTWS";
const MAX_CHECKPOINT: usize = 128 * 1024 * 1024;

/// Write the schema, attach protocol and bounded payload as one offer. A
/// partial offer cannot confer ownership; the candidate must vote separately.
pub fn write_checkpoint(mut writer: impl Write, payload: &[u8]) -> io::Result<()> {
    let length = payload.len();
    if length == 0 || length > MAX_CHECKPOINT {
        return Err(invalid("invalid Workspace checkpoint size"));
    }
    let mut header = [0; 16];
    header[..4].copy_from_slice(&MAGIC);
    header[4..8].copy_from_slice(&CHECKPOINT_VERSION.to_be_bytes());
    header[8..12].copy_from_slice(&uniterm_proto::WIRE_PROTOCOL_VERSION.to_be_bytes());
    header[12..].copy_from_slice(&(length as u32).to_be_bytes());
    writer.write_all(&header)?;
    writer.write_all(payload)
}

/// Read the complete offer from a private peer, rejecting incompatible
/// images before interpreting or allocating space for their payloads.
pub fn read_checkpoint(reader: impl Read) -> io::Result<Vec<u8>> {
    read_checkpoint_limited(reader, MAX_CHECKPOINT)
}

fn read_checkpoint_limited(mut reader: impl Read, limit: usize) -> io::Result<Vec<u8>> {
    // One small fixed read per header, including on fragmented streams.
    let mut header = [0; 16];
    reader.read_exact(&mut header)?;
    if header[..4] != MAGIC
        || header[4..8] != CHECKPOINT_VERSION.to_be_bytes()
        || header[8..12] != uniterm_proto::WIRE_PROTOCOL_VERSION.to_be_bytes()
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "incompatible Workspace checkpoint",
        ));
    }
    let length = u32::from_be_bytes(header[12..].try_into().unwrap()) as usize;
    if length == 0 || length > limit.min(MAX_CHECKPOINT) {
        return Err(invalid("invalid Workspace checkpoint size"));
    }
    let mut payload = Vec::new();
    reader.take(length as u64).read_to_end(&mut payload)?;
    if payload.len() != length {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(payload)
}

/// Duplicate one ordered batch of live resources into an already-connected
/// private channel. The source retains every original until ownership commit.
pub fn send_descriptors(
    channel: &UnixStream,
    fds: &[std::os::fd::BorrowedFd<'_>],
) -> io::Result<()> {
    if fds.is_empty() || fds.len() > MAX_RIGHTS {
        return Err(invalid("invalid handoff descriptor count"));
    }
    let raw: Vec<_> = fds.iter().map(AsRawFd::as_raw_fd).collect();
    loop {
        match sendmsg::<()>(
            channel.as_raw_fd(),
            &[IoSlice::new(b"F")],
            &[ControlMessage::ScmRights(&raw)],
            MsgFlags::empty(),
            None,
        ) {
            Ok(1) => return Ok(()),
            Ok(_) => return Err(io::ErrorKind::WriteZero.into()),
            Err(nix::errno::Errno::EINTR) => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

/// Receive an exact batch before starting any candidate workers or spawning
/// children. On rejection, close every descriptor the kernel installed.
pub fn receive_descriptors<const N: usize>(channel: &UnixStream) -> io::Result<[OwnedFd; N]> {
    if N == 0 || N > MAX_RIGHTS {
        return Err(invalid("invalid handoff descriptor count"));
    }
    let mut tag = [0u8];
    let mut ancillary = nix::cmsg_space!([RawFd; MAX_RIGHTS]);
    let mut iov = [IoSliceMut::new(&mut tag)];
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let flags = MsgFlags::MSG_CMSG_CLOEXEC;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let flags = MsgFlags::empty();
    let message = loop {
        match recvmsg::<()>(channel.as_raw_fd(), &mut iov, Some(&mut ancillary), flags) {
            Ok(message) => break message,
            Err(nix::errno::Errno::EINTR) => continue,
            Err(error) => return Err(error.into()),
        }
    };
    let mut valid = message.bytes == 1 && !message.flags.contains(MsgFlags::MSG_CTRUNC);
    let mut owned = Vec::with_capacity(N);
    for control in message.cmsgs() {
        if let ControlMessageOwned::ScmRights(fds) = control {
            for fd in fds {
                // SAFETY: recvmsg created each SCM_RIGHTS descriptor for this receiver.
                owned.push(unsafe { OwnedFd::from_raw_fd(fd) });
            }
        } else {
            valid = false;
        }
    }
    if !valid || tag != *b"F" || owned.len() != N {
        return Err(invalid("incomplete or unexpected handoff descriptor batch"));
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    for fd in &owned {
        // Darwin has no MSG_CMSG_CLOEXEC. The receiver is single-threaded
        // until all bootstrap descriptors are protected.
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    owned
        .try_into()
        .map_err(|_| invalid("wrong handoff descriptor count"))
}

/// The source keeps byte zero of its durable claim until it has fenced all
/// effects. Every path out of preparation closes and reaps the candidate
/// before rejoining the claim, except a successful one-byte commit.
pub struct SourceOffer<'a> {
    claim: &'a super::WorkspaceLock,
    candidate: &'a mut std::process::Child,
    channel: &'a mut UnixStream,
    ready: bool,
    finished: bool,
}

impl<'a> SourceOffer<'a> {
    /// Start once verification has yielded a private candidate socket and the
    /// running source has stopped dispatching work at an event-loop boundary.
    pub fn begin(
        claim: &'a super::WorkspaceLock,
        candidate: &'a mut std::process::Child,
        channel: &'a mut UnixStream,
    ) -> io::Result<Self> {
        // An imported engine owns at least the tail. Claim byte zero as well
        // before offering that tail to a subsequent generation.
        claim.handoff_range(libc::F_WRLCK as _, 0, false)?;
        let offer = Self {
            claim,
            candidate,
            channel,
            ready: false,
            finished: false,
        };
        offer.claim.handoff_range(libc::F_UNLCK as _, 1, false)?;
        Ok(offer)
    }

    /// Write the complete checkpoint over the candidate's original private channel.
    pub fn channel(&self) -> &UnixStream {
        self.channel
    }

    /// Only the exact ready byte permits commit. EOF, a rejection and an
    /// incompatible peer all leave the original engine entitled to resume.
    pub fn await_ready(&mut self) -> io::Result<()> {
        if self.ready {
            return Ok(());
        }
        let mut vote = [0];
        self.channel.read_exact(&mut vote)?;
        match vote[0] {
            b'R' => {
                self.ready = true;
                Ok(())
            }
            b'E' => {
                let reason = read_checkpoint_limited(&*self.channel, 4096)?;
                Err(io::Error::other(
                    String::from_utf8_lossy(&reason).into_owned(),
                ))
            }
            b'N' => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "replacement declined the live Workspace",
            )),
            _ => Err(invalid("invalid Workspace handoff vote")),
        }
    }

    /// Return success only after the irrevocable byte was accepted by the
    /// kernel. From this point the source must relinquish its listeners and
    /// claims, including when the candidate subsequently disconnects.
    pub fn commit(mut self) -> io::Result<()> {
        if !self.ready {
            return Err(invalid("replacement did not vote ready"));
        }
        self.channel.write_all(b"C")?;
        self.finished = true;
        Ok(())
    }

    /// Abort the exact child even if it became unresponsive while holding
    /// the tail lock. Reaping precedes restoration of the full source lock.
    pub fn rollback(mut self) -> io::Result<()> {
        self.cancel()
    }

    fn cancel(&mut self) -> io::Result<()> {
        let _ = self.channel.shutdown(std::net::Shutdown::Both);
        if self.candidate.try_wait()?.is_none() {
            self.candidate.kill()?;
            self.candidate.wait()?;
        }
        self.claim.handoff_range(libc::F_WRLCK as _, 0, false)?;
        self.finished = true;
        Ok(())
    }
}

impl Drop for SourceOffer<'_> {
    fn drop(&mut self) {
        if !self.finished {
            if let Err(error) = self.cancel() {
                // The source still owns byte zero, excluding all normal owners.
                eprintln!("uniterm: failed to restore full Workspace handoff claim: {error}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::fd::AsFd;

    #[test]
    fn checkpoint_header_rejects_truncation_unknown_versions_and_lengths_before_allocation() {
        let mut packet = Vec::new();
        write_checkpoint(&mut packet, b"state").unwrap();
        assert_eq!(read_checkpoint(packet.as_slice()).unwrap(), b"state");
        for len in 0..packet.len() {
            assert!(read_checkpoint(&packet[..len]).is_err());
        }
        for at in [0, 4, 8] {
            let mut incompatible = packet.clone();
            incompatible[at] ^= 1;
            assert_eq!(
                read_checkpoint(incompatible.as_slice()).unwrap_err().kind(),
                io::ErrorKind::Unsupported
            );
        }
        let header = packet.len() - b"state".len();
        for length in [0, u32::MAX] {
            packet[header - 4..header].copy_from_slice(&length.to_be_bytes());
            assert_eq!(
                read_checkpoint(packet.as_slice()).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
    }

    #[test]
    fn rejected_rights_are_closed_and_valid_rights_are_close_on_exec() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let (mut observer, transferred) = UnixStream::pair().unwrap();
        observer.set_nonblocking(true).unwrap();
        send_descriptors(&sender, &[transferred.as_fd(), transferred.as_fd()]).unwrap();
        drop(transferred);
        assert!(receive_descriptors::<1>(&receiver).is_err());
        assert_eq!(observer.read(&mut [0]).unwrap(), 0);

        let (mut observer, transferred) = UnixStream::pair().unwrap();
        send_descriptors(&sender, &[transferred.as_fd()]).unwrap();
        let [received] = receive_descriptors(&receiver).unwrap();
        assert_ne!(
            unsafe { libc::fcntl(received.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        UnixStream::from(received).write_all(b"R").unwrap();
        let mut data = [0];
        observer.read_exact(&mut data).unwrap();
        assert_eq!(&data, b"R");
    }
}
