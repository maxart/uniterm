//! Event-triggered discovery of HTTP listeners owned by a Pane's descendants.
//! No process-table scan or repeating timer: connectors call this after tools.

#[cfg(any(target_os = "linux", target_os = "android"))]
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::time::Duration;

pub(crate) fn discover(session: u32, providers: &crate::providers::Catalog) -> Vec<u16> {
    owned_ports(session, providers)
        .into_iter()
        .filter(|port| is_http(*port))
        .take(32)
        .collect()
}

// Only inspect the executable (or an interpreter's entry script), not arbitrary
// arguments such as a project directory named after an agent. Excluding the
// owner must never prune its children: those are the development servers.
fn infrastructure_listener(command: &str, providers: &crate::providers::Catalog) -> bool {
    let mut words = command.split_whitespace();
    let program = words.next().unwrap_or_default();
    let name = std::path::Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program);
    if matches!(name, "uniterm" | "ut") || providers.process(name).is_some() {
        return true;
    }
    if matches!(
        name,
        "node" | "nodejs" | "bun" | "deno" | "python" | "python3"
    ) {
        if let Some(script) = words.next().filter(|arg| !arg.starts_with('-')) {
            return providers.process(script).is_some();
        }
    }
    false
}

fn is_http(port: u16) -> bool {
    let timeout = Duration::from_millis(100);
    for ip in [
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(Ipv6Addr::LOCALHOST),
    ] {
        let Ok(mut stream) = TcpStream::connect_timeout(&SocketAddr::new(ip, port), timeout) else {
            continue;
        };
        let _ = stream.set_read_timeout(Some(timeout));
        let _ = stream.set_write_timeout(Some(timeout));
        let request =
            format!("HEAD / HTTP/1.0\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n");
        if stream.write_all(request.as_bytes()).is_err() {
            continue;
        }
        let mut prefix = [0; 5];
        if stream.read_exact(&mut prefix).is_ok() && &prefix == b"HTTP/" {
            return true;
        }
    }
    false
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn owned_ports(session: u32, providers: &crate::providers::Catalog) -> Vec<u16> {
    let mut sockets = HashMap::new();
    for table in ["tcp", "tcp6"] {
        if let Ok(text) = std::fs::read_to_string(format!("/proc/{session}/net/{table}")) {
            for line in text.lines().skip(1) {
                let fields: Vec<_> = line.split_whitespace().collect();
                if fields.len() > 9 && fields[3] == "0A" {
                    if let Some(port) = fields[1]
                        .rsplit_once(':')
                        .and_then(|(_, p)| u16::from_str_radix(p, 16).ok())
                    {
                        sockets.insert(format!("socket:[{}]", fields[9]), port);
                    }
                }
            }
        }
    }
    let mut pending = vec![session];
    let mut seen = HashSet::new();
    let mut ports = HashSet::new();
    while let Some(pid) = pending.pop() {
        if seen.len() >= 4096 || ports.len() >= 32 {
            break;
        }
        if !seen.insert(pid) {
            continue;
        }
        // Following children catches agent subprocesses with redirected output
        // and their own session, without mistaking unrelated host listeners.
        if let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) {
            for task in tasks.flatten().take(256) {
                if let Ok(children) = std::fs::read_to_string(task.path().join("children")) {
                    let capacity = 4096_usize.saturating_sub(seen.len() + pending.len());
                    pending.extend(
                        children
                            .split_whitespace()
                            .filter_map(|p| p.parse::<u32>().ok())
                            .take(capacity),
                    );
                }
            }
        }
        let command = std::fs::read(format!("/proc/{pid}/cmdline"))
            .map(|bytes| String::from_utf8_lossy(&bytes).replace('\0', " "))
            .unwrap_or_default();
        if infrastructure_listener(&command, providers) {
            continue;
        }
        if let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) {
            for fd in fds.flatten().take(4096) {
                if let Ok(path) = std::fs::read_link(fd.path()) {
                    if let Some(port) = sockets.get(path.to_string_lossy().as_ref()) {
                        ports.insert(*port);
                    }
                }
            }
        }
    }
    let mut ports: Vec<_> = ports.into_iter().collect();
    ports.sort_unstable();
    ports
}

#[cfg(target_os = "macos")]
fn owned_ports(session: u32, providers: &crate::providers::Catalog) -> Vec<u16> {
    let Ok(root) = i32::try_from(session) else {
        return Vec::new();
    };
    let mut pids = vec![root];
    let mut cursor = 0;
    let mut children = [0_i32; 256];
    while cursor < pids.len() && pids.len() < 4096 {
        // SAFETY: libproc receives a live, aligned pid buffer and its byte
        // capacity. It returns a count of initialized pid entries, clamped
        // before indexing. Only this known parent's children are requested.
        let count = unsafe {
            libc::proc_listchildpids(
                pids[cursor],
                children.as_mut_ptr().cast(),
                std::mem::size_of_val(&children) as i32,
            )
        }
        .max(0) as usize;
        for child in children.iter().take(count.min(children.len())) {
            if *child > 0 && !pids.contains(child) && pids.len() < 4096 {
                pids.push(*child);
            }
        }
        cursor += 1;
    }
    let selection = pids
        .iter()
        .map(i32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    // One event-triggered lookup scoped to descendants, never a process scan
    // or a recurring watcher. macOS supplies lsof with the base system.
    let Ok(output) = std::process::Command::new("/usr/sbin/lsof")
        .args([
            "-nP",
            "-a",
            "-p",
            &selection,
            "-iTCP",
            "-sTCP:LISTEN",
            "-Fn",
        ])
        .output()
    else {
        return Vec::new();
    };
    let mut excluded = false;
    let mut ports: Vec<_> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            // lsof always emits a process id before that process's sockets.
            if let Some(pid) = line.strip_prefix('p').and_then(|pid| pid.parse().ok()) {
                excluded = crate::runtime::process_command(pid)
                    .is_some_and(|command| infrastructure_listener(&command, providers));
                return None;
            }
            if excluded {
                return None;
            }
            line.strip_prefix('n')?
                .rsplit_once(':')?
                .1
                .parse::<u16>()
                .ok()
        })
        .collect();
    ports.sort_unstable();
    ports.dedup();
    ports.truncate(32);
    ports
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
fn owned_ports(_session: u32, _providers: &crate::providers::Catalog) -> Vec<u16> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infrastructure_owners_exclude_uniterm_and_providers_without_matching_project_arguments() {
        let providers = crate::providers::Catalog::load_from_paths(None, None, None);
        for command in [
            "/home/user/.cargo/bin/uniterm serve",
            "/usr/local/bin/ut attach",
            "/opt/codex --resume",
            "node /opt/@anthropic-ai/claude-code/cli.js",
        ] {
            assert!(infrastructure_listener(command, &providers), "{command}");
        }
        for command in [
            "",
            "python3 -m http.server",
            "node /work/uniterm/server.js",
            "node /work/app/server.js --root /work/codex",
            "uniterm-server-demo",
            "curl http://localhost:3000",
        ] {
            assert!(!infrastructure_listener(command, &providers), "{command}");
        }
    }
}
