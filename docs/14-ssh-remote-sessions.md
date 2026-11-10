# 14 - SSH Remote Sessions

This document records the transport design for attaching a local Uniterm client to a Uniterm server on another machine.

## Goals

- The remote server owns PTYs, grids, Workspaces, persistence, and agent state exactly as it does for a local attach.
- The local process owns raw terminal mode, keybindings, mouse parsing, overlays, and final terminal cleanup.
- A dropped SSH connection detaches only that client.
- No TCP listener, public socket, custom authentication system, or async work is added to the server.
- Backpressure remains bounded and never turns a temporary slow link into a repaint-induced detach.
- Binary protocol incompatibility is reported before the terminal enters raw mode.

## Prior art

tmux keeps its server protocol on a local Unix socket.
Its common remote workflow is to run the tmux client after logging in with SSH, so SSH carries terminal bytes rather than exposing the tmux socket.
tmux also versions every peer message and uses buffered event-driven writes.
Its control mode applies explicit watermarks and eventually disconnects a consumer that remains too far behind.

It starts a remote stdio bridge through `ssh -T`, proxies that bridge through a private local Unix socket, checks remote protocol compatibility, configures keepalives, and keeps the remote server independent of the SSH process.

Uniterm combines those ideas.
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
Both `uniterm` and `ut` must resolve on the remote non-interactive `PATH`.

The local client uses local keybindings and presentation settings.
The remote server uses remote server settings, persistence, Projects, panes, and provider integrations.
The Workspace picker is disabled during a remote attach because sibling sockets beside the private proxy are local implementation details, not remote Workspaces.
To change remote Workspaces, detach and run `ut remote HOST NAME`.

When a remote Uniterm is launched from inside a local Uniterm Pane, the inner client announces its input lifetime through a private OSC marker that passes unchanged through SSH.
The outer server then keeps mouse reports on the existing pane-relative SGR passthrough path and gives one configured prefix to the inner client.
Use the outer prefix twice before a command to target the outer Uniterm while the nested client is active.
For example, with the default prefix, `Ctrl-A o` opens the remote Observatory and `Ctrl-A Ctrl-A o` opens the local Observatory.
The exit marker, shell prompt recovery, and local foreground-process recovery all restore ordinary outer input priority after a clean exit or crash.

## Connection flow

1. The local CLI validates the SSH target and optional Workspace name.
2. It creates a mode `0700` directory under `/tmp` for one mode `0600` proxy socket.
3. While the terminal is still cooked, the proxy starts the one long-lived `ssh -T HOST <remote-bridge command>` data connection.
4. The remote binary compares the offered wire version with `WIRE_PROTOCOL_VERSION`.
5. The remote bridge creates or starts the Workspace server, connects to its owner-only Unix socket, writes a textual ready marker, then waits for framed client bytes.
6. The local proxy discards bounded login-shell text until it sees the ready marker.
7. Only after that successful handshake does the ordinary `uniterm-client` enter raw mode and connect to the private local Unix listener.
8. The proxy forwards framed protocol bytes in both directions over the already-established SSH process.
9. On explicit detach, the proxy and SSH child exit while the detached remote server continues running.
10. On network loss or unexpected EOF, the client restores the terminal and reports a non-zero failure instead of treating the disconnect as a clean detach.

The same nested-input negotiation applies to the traditional `ssh -t HOST uniterm WORKSPACE` workflow.
It uses terminal bytes rather than SSH implementation details, so the behavior is shared by Linux and macOS clients and by OpenSSH-compatible transports such as Teleport.

The hidden `remote-check` and `remote-bridge` commands are transport internals.
They are not network services and accept only the fixed protocol argument plus a validated Workspace name.

## Security properties

- The SSH target cannot begin with `-`, so user input cannot become an OpenSSH option.
- Workspace names use the existing restricted path-safe alphabet.
- Remote commands interpolate only a numeric protocol version and a validated Workspace name.
- The proxy socket lives in a newly created mode `0700` directory.
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

- Missing remote binary: the bridge handshake fails before raw mode and explains that `uniterm` or `ut` must be on the remote `PATH`.
- Wire mismatch: the bridge handshake reports both protocol versions and asks for matching builds.
- Authentication failure: OpenSSH reports it while the local terminal is still cooked.
- Login-shell noise: up to 64 KiB is discarded before the ready marker; a larger preamble aborts.
- Network loss: OpenSSH keepalives detect a dead connection, the local client restores its terminal, reports failure, and the remote server survives.
- Slow rendering: obsolete repaint deltas are coalesced and replaced by one current full repaint.

## Deliberate scope

The first release does not copy or install binaries on the remote host.
It does not expose local desktop-only file or image transfer.
It does not make the local Workspace picker enumerate remote sockets.
These can be added later without changing the server protocol transport or weakening the Unix-socket boundary.
