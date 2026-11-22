# Saved SSH Servers

The left sidebar has Local and Servers tabs at its bottom.
They share the Observatory tab strip, including theme colors, centered labels, truncation and hit rectangles.
Local is the default, and the normal Project hierarchy is unchanged above these controls.
Servers opens a native management dialog using the same theme, inputs, and overlay frame as other Uniterm management surfaces.
The Workspace menu and `ut servers` open the same surface.

## Saved profiles and discovery

A saved Server contains a stable numeric identity, an editable label, an SSH target, a default remote Workspace, and an enabled flag.
Authentication, keys, ports, jump hosts, and host trust remain OpenSSH responsibilities.
No credentials are stored in Uniterm's catalog.

Press `a` to add a Server, `n` to edit its details, `e` to toggle its enabled state, or `d` and then `y` to forget its profile.
Removing or disabling a profile does not stop remote Workspaces or agents.
Arrow keys select a profile.
Press `r` to query its remote Workspaces, including running/stopped state and Project, Tab, and Pane counts.
The result is labelled a snapshot, because it is an observation from that explicit query rather than an ongoing online guarantee.
`Tab` selects a discovered Workspace and `Enter` connects; without discovery, Enter connects to the saved default Workspace.

Discovery runs outside the server core loop, wakes the client on completion, and has a 20-second deadline and a 1 MiB reply limit.
It uses noninteractive SSH and strict host-key verification.
Closing the dialog, selecting another Server, or returning Local cancels its discovery child.
Authentication failures and protocol incompatibilities appear as Attention messages.
Interactive connection runs after terminal raw mode has been restored so OpenSSH can handle authentication and host verification normally.

## Switching and recovery

Selecting a Server connects through the existing SSH stdio bridge.
The remote server continues to own its terminals, agents, persistence, and Workspace state.
Remote IDs never become local IDs, and a command issued from a Pane retains that Pane's original Workspace scope.
The Servers dialog always reads the client machine's saved catalog, including while viewing a remote Workspace.

Select Local in the sidebar or press `l` in Servers to return to the original local Workspace.
A failed connection or an interrupted active SSH connection returns a client that started locally to its original local Workspace and reopens Servers with the error.
Enter reconnects explicitly; no failed mutation or agent prompt is automatically repeated.
Remote sessions remain running when the client switches away.
Only the selected machine has an attached screen connection; the feature does not claim a background federated fleet or cross-machine notifications.
There are no idle discovery timers, filesystem scans, or background authentication attempts.

## Durable machine catalog

Machine profiles are machine-scoped, rather than part of one Workspace.
Their ground truth is a separate append-only event stream at `$XDG_STATE_HOME/uniterm/machines.jsonl`, falling back to `~/.local/state/uniterm/machines.jsonl`.
Each versioned record has a monotonic sequence and a saved/removed profile event.
Writers acquire an advisory sidecar lock, replay and validate the latest stream, append and sync the event, then expose success.
Read-only listing never creates a catalog.
The catalog and lock are created with mode `0600`.
An unreadable, invalid, or torn catalog produces an error and is preserved rather than silently replaced.
A maximum of 128 profiles and a 16 MiB stream bound prevent an unbounded management projection.

## Commands

```sh
ut servers
ut machine list --json
ut machine add "Build server" user@build agents
ut machine rename 1 "Build server west"
ut machine disable 1
ut machine enable 1
ut machine sessions 1 --json
ut machine status 1 --json
ut machine connect 1
ut machine reconnect 1
ut machine remove 1
```

`status` and `sessions` perform one fresh discovery request.
`connect` and `reconnect` use the same interactive attach path.
The control API `servers_open` opens the client-local dialog; machine profile CRUD is deliberately performed on the machine running the client through the CLI or dialog, rather than on a selected remote server.
The hidden `machine-catalog` helper reports protocol version and bounded host Workspace metadata for discovery.
It never starts a stopped Workspace.

## Verification

`machine_catalog_appends_replays_and_preserves_invalid_history` covers no-op reads, event replay and invalid-history preservation.
`saved_machines_crud_is_event_backed_and_reads_do_no_work` covers isolated CLI metadata, file permissions and identity stability across removal.
`saved_machine_discovery_reports_remote_sessions_without_starting_them` covers SSH discovery, running/stopped sessions and visible authentication errors using a fake SSH executable.
The transport's existing SSH handshake, disconnection, and terminal recovery tests remain applicable to saved connections.

`servers_native_pty_add_edit_disable_local_and_idle` exercises add, edit, disable, remove, Local return, and zero idle bytes through a real attach PTY.
`shared_navigation_tabs_match_observatory_styles_and_route_without_network` checks first attach and exact click targets in compact and narrow sidebars.
`cancelling_discovery_reaps_its_process_group_and_joins_readers` checks cancellation cleanup without process polling.
