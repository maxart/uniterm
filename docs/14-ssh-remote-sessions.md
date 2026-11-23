# 14 - SSH Remote Sessions

This document records the transport design for attaching a local Uniterm client to a Uniterm server on another machine.

## Goals

- The remote server owns PTYs, grids, Workspaces, persistence, and agent state exactly as it does for a local attach.
- The local process owns raw terminal mode, keybindings, mouse parsing, overlays, and final terminal cleanup.
- A dropped SSH connection detaches only that client.
- No TCP listener, public socket, custom authentication system, or async work is added to the server.
- Backpressure remains bounded and never turns a temporary slow link into a repaint-induced detach.
- Binary protocol incompatibility is reported before the client switches Workspaces.

## Prior art

tmux keeps its server protocol on a local Unix socket.
Its common remote workflow is to run the tmux client after logging in with SSH, so SSH carries terminal bytes rather than exposing the tmux socket.
tmux also versions every peer message and uses buffered event-driven writes.
Its control mode applies explicit watermarks and eventually disconnects a consumer that remains too far behind.

The server remains Unix-socket-only like tmux, while the optional local-client workflow uses an SSH stdio bridge.

## User interface

```sh
ut remote workbox
ut remote workbox agents
ut --remote workbox agents
```

The first command uses the remote machine's configured default Workspace.
The second creates or attaches to the named remote Workspace.
OpenSSH targets such as `user@host` are accepted, and aliases, ports, jump hosts, and identities should be configured in `~/.ssh/config`.
Install `uniterm` or `ut` on the remote SSH, login profile or interactive shell `PATH`.
The bridge and saved-server discovery use the same login shell environment as an interactive terminal and retain directories supplied by SSH.
The Workspace can discover providers installed by interactive shell initialization even when Uniterm itself is installed system-wide.
A private pipe carries the exported login environment back to the launcher.
Shell initialization has no protocol input, and its stdout and stderr are discarded before Uniterm starts.

The local client uses local keybindings and presentation settings.
The remote server uses remote server settings, persistence, Projects, panes, and provider integrations.
The Workspace picker queries the remote host catalog by name, never local sockets beside the private bridge.
Saved connections are available through the [Local/Servers navigator](32-saved-servers.md).

When a remote Uniterm is launched from inside a local Uniterm Pane, the inner client announces its input lifetime through a private OSC marker that passes unchanged through SSH.
The outer server then keeps mouse reports on the existing pane-relative SGR passthrough path and gives one configured prefix to the inner client.
Use the outer prefix twice before a command to target the outer Uniterm while the nested client is active.
For example, with the default prefix, `Ctrl-A o` opens the remote Observatory and `Ctrl-A Ctrl-A o` opens the local Observatory.
The exit marker, shell prompt recovery, and local foreground-process recovery all restore ordinary outer input priority after a clean exit or crash.

## Connection flow

1. The local CLI validates the SSH target and optional Workspace name.
2. The proxy starts one long-lived `ssh -T HOST <remote-bridge command>` data connection.
   Direct CLI entry leaves the terminal cooked for OpenSSH.
   An attached client retains its current screen and socket, and gives OpenSSH a private controlling terminal for prompts.
3. The remote binary compares the offered wire version with `WIRE_PROTOCOL_VERSION`.
4. The remote bridge creates or starts the Workspace server, connects to its owner-only Unix socket, writes a textual ready marker, then waits for framed client bytes.
5. The local proxy discards bounded login-shell text up to the ready marker, then requests the Workspace's window and pane counts on the same stream.
   It checks the Workspace identity when supplied by the bridge and requires a valid server reply before committing the switch.
   Each probe reply is decoded as one exact frame of at most 1 KiB.
   Any bytes the Workspace sent behind its reply already belong to the attach stream; the proxy keeps them and delivers them to the client after the identity frame and before later bridge output.
6. Only after that handshake does the proxy create a mode `0700` directory under `/tmp` holding one mode `0600` listener.
   A cancelled, failed or timed-out authentication therefore leaves nothing in `/tmp`.
   The name is `uniterm-ssh-<pid>-<n>`, where `<n>` never repeats within a process, so the two bridges of a remote-to-remote switch cannot share or inherit a name.
7. The client connects to that listener, which accepts exactly one connection.
   The bridge worker owns the name: as soon as the connection is accepted it closes the listener and removes the socket and directory, once.
   For the rest of the attach no pathname exists, so a client ended by a terminal hangup or a signal leaves no file behind; only the short interval between creating the listener and accepting its client is exposed to that.
   A bridge whose client never arrives is woken over a private descriptor, never by connecting to a pathname.
   An existing attachment releases its source connection and keeps the physical terminal in raw mode.
8. The proxy forwards framed protocol bytes in both directions over the already-established SSH process.
9. On explicit detach, the proxy and SSH child exit while the detached remote server continues running.
10. On unexpected EOF, a client that began locally reopens Servers on its local Workspace with the error.
    A standalone remote client restores the terminal and exits with a failure.

The same nested-input negotiation applies to the traditional `ssh -t HOST uniterm WORKSPACE` workflow.
It uses terminal bytes rather than SSH implementation details, so the behavior is shared by Linux and macOS clients and by OpenSSH-compatible transports such as Teleport.

The hidden `remote-check` and `remote-bridge` commands are transport internals.
They are not network services and accept only the fixed protocol argument plus a validated Workspace name.

## Security properties

- The SSH target cannot begin with `-`, so user input cannot become an OpenSSH option.
- Workspace names use the existing restricted path-safe alphabet.
- Remote commands interpolate only a numeric protocol version and a validated Workspace name.
- The proxy socket lives in a newly created mode `0700` directory, exists only between a successful handshake and the client's connection, and is never reused.
- Authentication and host verification remain OpenSSH responsibilities.
- The remote Uniterm socket remains owner-only and is never forwarded as a listening TCP port.
- Login banners are bounded and removed before the binary frame decoder sees them.
- The single SSH process is owned for exactly the lifetime of the remote attach.

## Repaint and terminal recovery

An SSH link or a large local terminal can drain render output more slowly than panes generate it.
Uniterm never removes a partially written frame.
When a client already has render bytes pending, later damage is collapsed into a `repaint_pending` bit.
After writable readiness drains the older frame, the server sends one authoritative full repaint from its current grids.
This preserves the fixed memory bound without disconnecting a healthy client merely because intermediate scroll frames became obsolete.

Normal full-screen applications restore the primary buffer with DEC private modes 47, 1047, or 1049.
If a local application is killed before doing so, the kernel-observed foreground process transition back to the pane shell restores the primary buffer and shell input modes.
Across SSH the local kernel cannot see remote process groups, so OSC 133 command-finished and prompt-start marks provide the provider-neutral recovery signal.
OSC 777 `session_end` remains an additional cooperative recovery path for connected agents.

## Failure behavior

- Missing remote binary: the bridge handshake fails and the diagnostic identifies the missing executable.
- Wire mismatch: the bridge handshake reports both protocol versions and asks for matching builds.
- Authentication and host verification use OpenSSH. During a switch the client displays its private terminal prompt inside Uniterm.
  Escape, Ctrl-C or the Cancel footer restores Servers on the current attachment.
- Handshake time: the peer has 120 seconds in total to reach the ready marker and answer the probe, however much it prints.
  A question OpenSSH leaves open on its private terminal (a host key confirmation, a password, a second factor) suspends that budget, because reading a fingerprint or fetching a code is the person's time and not the host's.
  A prompt is open only while the cursor waits on a line with visible text; the line ending OpenSSH writes after an answer is not a prompt.
  Each open prompt has its own 5 minute bound, renewed only by a local keystroke.
  Remote output, further prompt text and pointer reports never renew it, and at most 16 prompts are accepted before the handshake.
  When either bound expires the switch fails with its own message and Servers returns on the current attachment.
- Login-shell stdout is discarded up to the 64 KiB ready-marker limit. During a switch stderr is drained separately into a bounded diagnostic tail.
  Standalone SSH entry also displays sanitized stderr instructions on the cooked terminal until the handshake completes.
- Network loss: OpenSSH keepalives detect a dead connection.
  Only the client disconnects and the remote Workspace survives.
- Slow rendering: obsolete repaint deltas are coalesced and replaced by one current full repaint.

## Deliberate scope

The first release does not copy or install binaries on the remote host.
It does not expose local desktop-only file or image transfer.
It does not make the local Workspace picker enumerate remote sockets.
These can be added later without changing the server protocol transport or weakening the Unix-socket boundary.
