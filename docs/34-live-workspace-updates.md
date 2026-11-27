# Live Workspace updates

A user should be able to apply a release from an open Workspace and keep typing into the same shell.
A running agent should retain its PID, terminal, invocation, permission state and in-flight work.
The version on disk, the attached client and the server owning a Workspace can each be different.
The implementation record is in [STATUS.md](STATUS.md).

## Client activation

Installation keeps its existing consent, release verification and binary-pair rollback.
After the menu finishes installing, the client can replace its own process image in cooked terminal mode and reattach.
It reacquires the install lock, rechecks both executable files against the release manifest, and holds the lock through the version check, attach protocol check and exec.
Post-install connector refresh uses the same lock and checksum checks.
A local handoff carries the selected Workspace, including a Workspace selected after the original attach.
An SSH handoff carries the current host, Workspace and local return destination.
The remote host controls updates to its own installation.

The JSON control socket provides the running server's build identity and attach protocol on demand.
Older servers omit the identity.
A client that just used that connection can use its own attach protocol as the compatibility requirement for a legacy server.
An incompatible candidate leaves the existing client available for that session.
Running the updater inside a pane can replace the installation but cannot replace the outer attach process directly.

## Live server activation

Run `ut update --activate-server --workspace NAME` after installing a release on the Workspace's host.
A Workspace with a compatible engine accepts a release number, resolves its own installation directory and fetches the public checksum manifest independently of the requesting client.
It verifies both executables under the installer lock before starting the replacement on a private socket.
Verification, journal synchronization and listener preparation run in the background while the current engine continues handling keystrokes.
New runtime work during preparation cancels the attempt and reopens admissions.
Only the final capture, descriptor transfer and vote pause terminal service, under a bounded transfer deadline.
The installer and the verifier lock the installation directory inode, allowing a normal user to activate binaries installed by an administrator.
After commit, the replacement answers the initiating client with its running version.
A verified image that is already serving returns its version without transferring the Workspace.
A missing acknowledgement reports an unknown outcome (`activated: null` in JSON).
Use `ut update --status --workspace NAME --json` to check the owner in that case.

### Transfer and rollback

The source passes a PTY master, the listening socket, both Workspace claims and an accepted client socket to a separate candidate over a private socketpair using `SCM_RIGHTS`.
The private transport checks the descriptor count, closes rejected descriptors, and checks the checkpoint format, attach protocol and payload size before reading the body.
The candidate validates a bounded checkpoint, restores a pending permission prompt and waits for commit before consuming requests.
The source retains authority to resume if the candidate rejects the checkpoint or dies before commit.

The socket claim uses `flock` and follows its open file description.
The durable claim is a POSIX record lock owned by a PID.
The source retains an exclusive lock on byte zero while the candidate acquires the range from byte one through EOF.
If the candidate declines, sends an unknown vote, closes its channel, or refuses the one-byte commit, the source cancels and reaps that candidate before extending its claim again.
After commit the candidate retains its already acquired tail lock and waits for a private channel signal when the old acceptors and transcript observer retire.
Unrelated background worker shutdown can finish after the replacement starts serving.
It can continue serving without another lock acquisition.
Before a later transfer the new source reacquires the full range while retaining the tail, then offers byte one onward to its replacement.
Every full-file claim attempted by an ordinary competing process conflicts with at least one owner during this exchange.
A contender checks both the public socket path and an alternate runtime directory sharing the durable lock inode.

Run `cargo test -p uniterm-server --lib pending_approval_survives_two_process_handoff`.
The shell fixture records its PID and each input byte.
The client splits an approval request across the transfer and receives replies on its original connection, including a frame the previous owner had only partially written.
The committed engine uses the normal server loop, waiting action, PTY input queue, and event writer.
A partial PTY write retains the original byte offset and registers writable interest before commit so a child receives the remaining bytes even if the user never types another key.
The live layout validates Project and Tab ownership, split and stack relationships, viewport geometry and retired identity ranges before installation.
The candidate installs it without resizing any shell or perturbing an application with a window-change signal.
The runtime synchronizes the journal in request order, and the candidate checks its exact cursor before voting.
Both workers append to the same durable stream.
Before offering a checkpoint, the runtime pauses its general, read-only manager and restricted inbox acceptors.
An established JSON connection, pending request or subscription defers the handoff and reopens every prepared acceptor.
The dispatcher also defers while a detached operation is still running, including an external editor, worktree change or helper lookup.
The busy-editor case holds a real child on a FIFO, finishes its request on the original server, then retries.
A connection that arrives while the acceptors are paused stays in its original kernel backlog.
The candidate checks the bound address, socket type, pathname ownership, mode, device and inode of each listener before committing.
A renamed attach socket retains its original kernel address while the new filesystem path must still identify the same inode.
Accepted client streams are checked against that original address.
The candidate registers its listeners and creates its runtime before voting ready.
It waits for commit before accepting, deleting socket names, or dispatching runtime effects.
A graceful candidate abort after registration leaves the source's sockets, queued clients and running shell intact.
After commit it accepts the queued requests on the inherited sockets with their original permissions.
Rollback resumes the same acceptors, and a stale transaction cannot reopen another transaction's listeners.
The private Workspace checkpoint preserves the inbox epoch, journal, messages, claims, deduplication keys, rate limits, grants and revocations.
The destination rebuilds indexes and applies the original expiry deadlines before servicing any queued request.
It rejects a checkpoint for another Workspace.
The inode of the listening socket itself is checked separately from its pathname, including on macOS.

The cases cover commit, candidate death before commit, incompatible schema, stale event cursor, invalid client and Pane offsets, swapped control scopes, mismatched prompt fingerprint, an already consumed approval, source exit and a multi-step selection.
In the multi-step case, the first engine delivers a navigation key and the second processes the provider's redraw before sending Enter.
A pending output wait completes on its original client connection.
The fixtures exercise Codex and Claude prompts.

The live terminal checkpoint carries both grids, scrollback, damage, terminal modes, pending responses, and any unfinished parser sequence.
The parser exports its own bounded state on request, including its partial UTF-8 scalar.
Ordinary PTY reads record no input for handoff.
A long unfinished DCS can transfer without a second copy of its payload or a wait for a parser boundary.
The replacement validates the saved ranges and resumes at the next byte without delivering earlier callbacks.
Run `cargo test -p uniterm-server --lib live_checkpoint` for byte-by-byte interruption coverage, including UTF-8, OSC and alternate screens.

The production coordinator streams up to 4,096 Panes and all accepted attach clients, with a 128 MiB per-frame and 512 MiB aggregate checkpoint limit.
One terminal checkpoint, including scrollback, can use up to 64 MiB.
An oversized terminal reports its Pane ID and the limit while the original server continues serving.
Reducing retained scrollback or closing that Pane allows another attempt.
It preserves the hierarchy, user configuration, live model and exact client and terminal byte offsets.
The native session watcher preserves provider identities, deduplication records, unread transcript fragments and offsets.
The replacement registers the original watched directories before voting, then reconciles writes from the overlap at commit.
Aborting instead resumes the existing watched directories and their queued events.

A closing or recovering Pane, unresolved launch, active workflow or relay, Overwatch operation, tracked development server, live JSON connection, or outstanding runtime work can defer a transfer.
Close a transient Today view or finish a pointer gesture before retrying.
A refused update returns the reason while the original Workspace keeps running.

## Child supervision and idle cost

A same-PID exec preserves kernel parenthood.
The test `live_pty_and_workspace_claims_survive_exec` verifies the original shell and foreground job, an in-progress line of input, the public listener, both claims and the ability to collect the original shell’s exact exit status repeatedly.
The resumed PTY wrapper verifies the master descriptor and kernel parenthood before accepting a child.
A successful exec discards the previous image, which complicates recovery if initialization then fails.

A persistent owner could hold listener and PTY recovery descriptors and create and reap children across engine generations.
The active engine would still read and write PTYs directly through mio.
This costs a process per Workspace and an extra descriptor per Pane.
Uniterm targets direct transfer so ordinary Workspaces keep their current process count and keystroke path.
A replacement observes an imported shell's exit with a kernel watch, without starting a helper or querying the process table while idle.
It reports the exit cause as unknown because only the original parent can collect the status.
On Linux and Android the imported shell requires identity-bound process-group signalling, available since Linux 6.9.
An older kernel can continue serving the original Workspace.
On macOS explicit teardown captures the kernel lifetime identity of each process in the shell and foreground groups.
Before sending a signal it checks that identity and obtains the current audit-token version, which can change on exec.
At escalation it can also discover new members of a group while the PTY session or a previously captured member still proves ownership.
The regression `imported_foreground_shutdown_preserves_identity_and_unknown_exit_status` covers a job with a TERM-resistant descendant and rejects a mismatched session leader.
The regression `imported_shutdown_follows_an_anchor_through_exec_to_a_child_born_after_term` exercises an executable change and a new child during the grace period.
Shells it starts itself keep their usual wait status.
An owner must start the shell itself to preserve its wait status and retain the session leader until process-group shutdown is complete.
The old engine must retain the ability to resume before commit.
After commit, a surviving master descriptor keeps a shell alive, but cannot reveal which input bytes reached it.
The recovery protocol must account for those writes before retrying an approval or command.

## Ownership boundary

The engine pauses at a mio event-loop boundary and fences runtime admission and ordered persistence.
A candidate receives one Pane at a time, validates every resource and registers kernel interests while dormant.
Only its exact ready vote permits the source to send the commit byte.
Before that byte the source can cancel and resume its original poller.
After commit the source retires, even if the replacement crashes before answering the updater.
Replaying the previous checkpoint at that point could duplicate keystrokes or approvals.
Normal crash recovery uses the existing event log and snapshots.

## Test and measure

`cargo test -p uniterm-cli --test live_server_update` transfers two shells to the CLI executable and queries the replacement through a client connected to the previous engine.
`scripts/test-live-server-update.sh` compiles a separate release-stamped image from the committed source and exercises the public activation command against isolated release metadata.
Its mismatching alias checksum must keep the source serving.
Its committed update must preserve the original PID and an unexported shell variable.
`cargo test -p uniterm-server --lib handoff_preserves_transcript_offsets` covers a partial transcript line across commit and rollback.
The Ubuntu and Apple Silicon macOS CI gates run the independent-image test.
`cargo run --release -p uniterm-server --example checkpoint_spike` reports terminal checkpoint bytes, capture plus import time and transient heap for 20 and 45 Panes.
These component times exclude listener quiescence, socket transfer and process startup.
