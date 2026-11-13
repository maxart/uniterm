# Security policy

Uniterm runs shells and agent processes on your behalf and exposes a control socket to local automation, so security reports are taken seriously.

## Reporting

Please do not open a public issue for a vulnerability.
Report it privately through GitHub's security advisory form for this repository, or by email to the maintainer address listed on the GitHub profile of the repository owner.
You will get an acknowledgement within a few days and a fix or a mitigation plan as soon as one exists.

## Scope

In scope:

- The client-server protocol and the control socket under `$XDG_RUNTIME_DIR/uniterm/`.
- SSH remote attach (`ut remote`).
- Persistence under `$XDG_STATE_HOME/uniterm/`, including the event stream and snapshots.
- Escape-sequence handling: anything a child process can write that reaches a title, a notification, the clipboard, or another client.
- Provider detection manifests and their cache verification.

Out of scope:

- The behaviour of the agent CLIs themselves, or of commands you run inside a Pane.
- Systems where another user already has write access to your runtime or state directories.

## Design notes

Sockets, lock files, snapshots, and manifests are created with mode 0600 inside 0700 directories.
Every value that can reach terminal chrome is sanitised at the output boundary.
Destructive and bulk control-API methods require an explicit `confirmed: true`.
