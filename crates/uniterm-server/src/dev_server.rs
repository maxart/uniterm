//! Runtime-side detection and liveness for local development web servers.
//!
//! Detection consumes event-driven terminal screen tails. The mio core never
//! scans sockets or waits on a network call. Once an announce line is found,
//! the tokio runtime arms a bounded loopback probe and disarms it as soon as
//! the server goes away.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use uniterm_proto::DetectedDevServer;

const PROBE_TIMEOUT: Duration = Duration::from_millis(150);
#[cfg(not(test))]
pub(crate) const PROBE_INTERVAL: Duration = Duration::from_secs(5);
#[cfg(test)]
pub(crate) const PROBE_INTERVAL: Duration = Duration::from_millis(20);
pub(crate) const REFUSALS_BEFORE_DOWN: u8 = 3;

/// Result of one dual-stack loopback liveness check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PortProbe {
    Listening,
    Refused,
    Inconclusive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConnectProbe {
    Connected,
    Refused,
    Unavailable,
    Inconclusive,
}

/// Consecutive refusal tracking for one announced server.
///
/// A timeout, interruption, resource shortage, or failed blocking task is not
/// evidence that a listener exited. This distinction matters after host sleep
/// and under load, when a transient probe failure must not permanently remove
/// an otherwise quiet server from the event-driven projection.
pub(crate) struct LivenessState {
    refusals: u8,
}

impl LivenessState {
    pub(crate) fn new() -> Self {
        Self { refusals: 0 }
    }

    /// Record one observation and report whether absence is now confirmed.
    pub(crate) fn observe(&mut self, probe: PortProbe) -> bool {
        match probe {
            PortProbe::Listening | PortProbe::Inconclusive => self.refusals = 0,
            PortProbe::Refused => {
                self.refusals = self.refusals.saturating_add(1);
            }
        }
        self.refusals >= REFUSALS_BEFORE_DOWN
    }
}

/// Detect all distinct loopback servers announced in a terminal screen tail.
pub(crate) fn detect_servers(tail: &str) -> Vec<DetectedDevServer> {
    let mut ports = HashSet::new();
    tail.lines()
        .filter_map(detect_server)
        .filter(|server| ports.insert(server.port))
        .collect()
}

fn detect_server(line: &str) -> Option<DetectedDevServer> {
    let trimmed = line.trim();
    if trimmed.is_empty() || is_client_command(trimmed) {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();

    if lower.contains("serving http on") && lower.contains(" port ") {
        let port = number_after(&lower, " port ")?;
        return server("http.server", "http", "localhost", port, true);
    }

    if lower.contains("* listening on tcp://") {
        let (host, port) = tcp_authority_after(&lower, "tcp://")?;
        return server("rails", "http", &host, port, true);
    }

    let (scheme, host, port, explicit_port) = loopback_http_origin(trimmed)?;
    let label = classify(&lower)?;
    server(label, scheme, &host, port, explicit_port)
}

fn classify(lower: &str) -> Option<&'static str> {
    if lower.contains("[webpack-dev-server]") || lower.contains("project is running at") {
        Some("webpack-dev-server")
    } else if lower.contains("starting development server at") {
        Some("django")
    } else if lower.contains("uvicorn running on") {
        Some("uvicorn")
    } else if lower.contains("listening at:") {
        Some("gunicorn")
    } else if lower.contains("access ") && lower.contains(" at http") {
        Some("phoenix")
    } else if lower.contains("server running on [") {
        Some("laravel")
    } else if lower.contains("web server is available at") {
        Some("hugo")
    } else if lower.contains("server address:") {
        Some("jekyll")
    } else if lower.contains("[11ty]") && lower.contains("server at") {
        Some("11ty")
    } else if lower.contains("server running at") {
        Some("parcel")
    } else if lower.contains("bun") && (lower.contains(" on http") || lower.contains(" at http")) {
        Some("bun")
    } else if lower.trim_start().starts_with("-> ")
        && (lower.contains(".localhost") || lower.contains(".test"))
    {
        Some("portless")
    } else if lower
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .any(|label| label == "localdev")
    {
        Some("server")
    } else if lower.contains("* running on") {
        Some("flask")
    } else if lower.contains("* listening on http") {
        Some("rails")
    } else if lower.contains(" - local:") || lower.starts_with("- local:") {
        Some("next")
    } else if local_announce(lower) {
        Some("vite")
    } else if generic_announce(lower) {
        Some("server")
    } else {
        None
    }
}

fn local_announce(lower: &str) -> bool {
    lower.find("local").is_some_and(|at| {
        lower[at + "local".len()..]
            .trim_start_matches(|c: char| c == ':' || c.is_whitespace())
            .starts_with("http")
    })
}

fn generic_announce(lower: &str) -> bool {
    let line = lower
        .trim_start_matches(|c: char| c.is_whitespace() || "*>+|-".contains(c))
        .trim_start_matches(['➜', '•', '·', '▶', '▸', '▲'])
        .trim_start();
    [
        "listening",
        "running",
        "started",
        "ready",
        "serving",
        "available",
    ]
    .iter()
    .any(|verb| {
        line.starts_with(verb)
            || line
                .split_whitespace()
                .take(3)
                .any(|word| word.trim_matches(|c: char| !c.is_ascii_alphabetic()) == *verb)
    }) && (line.contains(" on http") || line.contains(" at http"))
}

fn is_client_command(line: &str) -> bool {
    let line = line
        .trim_start_matches(|c: char| c.is_whitespace() || "$>#❯".contains(c))
        .trim_start();
    let line = line.strip_prefix("sudo ").unwrap_or(line);
    let command = line.split_whitespace().next().unwrap_or_default();
    matches!(
        command.to_ascii_lowercase().as_str(),
        "curl" | "wget" | "ping" | "nc" | "netcat" | "nmap" | "telnet" | "http" | "httpie" | "xh"
    )
}

fn loopback_http_origin(line: &str) -> Option<(&'static str, String, u16, bool)> {
    let lower = line.to_ascii_lowercase();
    let mut offset = 0;
    while offset < lower.len() {
        let http = lower[offset..].find("http://").map(|at| (at, "http", 7));
        let https = lower[offset..].find("https://").map(|at| (at, "https", 8));
        let Some((at, scheme, prefix)) = (match (http, https) {
            (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }) else {
            break;
        };
        let start = offset + at + prefix;
        if let Some(authority) = parse_loopback_authority(&line[start..]) {
            let default = if scheme == "https" { 443 } else { 80 };
            return Some((
                scheme,
                authority.0,
                authority.1.unwrap_or(default),
                authority.1.is_some(),
            ));
        }
        offset = start;
    }
    None
}

fn parse_loopback_authority(value: &str) -> Option<(String, Option<u16>)> {
    let lower = value.to_ascii_lowercase();
    let (raw_host, rest) = if lower.starts_with('[') {
        let end = lower.find(']')?;
        (&lower[..=end], &lower[end + 1..])
    } else {
        let end = lower
            .find(|c: char| c == ':' || c == '/' || c.is_whitespace() || c == ']')
            .unwrap_or(lower.len());
        (&lower[..end], &lower[end..])
    };
    let host = match raw_host {
        "localhost" | "127.0.0.1" | "0.0.0.0" | "[::1]" | "[::]" => "localhost".into(),
        custom
            if custom.ends_with(".localhost")
                || custom.ends_with(".test")
                || custom.split('.').any(|label| label == "localdev") =>
        {
            custom.into()
        }
        _ => return None,
    };
    let port = rest.strip_prefix(':').and_then(leading_port);
    if rest.starts_with(':') && port.is_none() {
        return None;
    }
    Some((host, port))
}

fn tcp_authority_after(line: &str, marker: &str) -> Option<(String, u16)> {
    let value = line.split_once(marker)?.1;
    let (host, port) = parse_loopback_authority(value)?;
    Some((host, port?))
}

fn number_after(line: &str, marker: &str) -> Option<u16> {
    leading_port(line.split_once(marker)?.1)
}

fn leading_port(value: &str) -> Option<u16> {
    let digits: String = value.chars().take_while(char::is_ascii_digit).collect();
    let port = digits.parse::<u16>().ok()?;
    (port > 0).then_some(port)
}

fn server(
    label: &str,
    scheme: &str,
    host: &str,
    port: u16,
    explicit_port: bool,
) -> Option<DetectedDevServer> {
    (port > 0).then(|| DetectedDevServer {
        label: label.into(),
        url: if explicit_port {
            format!("{scheme}://{host}:{port}")
        } else {
            format!("{scheme}://{host}")
        },
        port,
    })
}

/// Probe both loopback address families without blocking the core loop.
///
/// Only an explicit refusal from every usable address family proves that the
/// port is down. Treating every I/O or task error as `false` makes transient
/// macOS resume and resource errors indistinguishable from a closed port.
pub(crate) async fn probe_port(port: u16) -> PortProbe {
    tokio::task::spawn_blocking(move || {
        let probes = [
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
        ]
        .into_iter()
        .map(|ip| connect_probe(SocketAddr::new(ip, port)));
        combine_connect_probes(probes)
    })
    .await
    .unwrap_or(PortProbe::Inconclusive)
}

fn connect_probe(address: SocketAddr) -> ConnectProbe {
    match std::net::TcpStream::connect_timeout(&address, PROBE_TIMEOUT) {
        Ok(_) => ConnectProbe::Connected,
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
            ConnectProbe::Refused
        }
        // A host can have one loopback address family disabled. That family
        // is neutral as long as the other one gives a definitive result.
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::AddrNotAvailable
                    | std::io::ErrorKind::NetworkUnreachable
                    | std::io::ErrorKind::Unsupported
            ) =>
        {
            ConnectProbe::Unavailable
        }
        Err(_) => ConnectProbe::Inconclusive,
    }
}

fn combine_connect_probes(probes: impl IntoIterator<Item = ConnectProbe>) -> PortProbe {
    let mut refused = false;
    let mut inconclusive = false;
    for probe in probes {
        match probe {
            ConnectProbe::Connected => return PortProbe::Listening,
            ConnectProbe::Refused => refused = true,
            ConnectProbe::Unavailable => {}
            ConnectProbe::Inconclusive => inconclusive = true,
        }
    }
    if refused && !inconclusive {
        PortProbe::Refused
    } else {
        PortProbe::Inconclusive
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(line: &str) -> DetectedDevServer {
        detect_server(line).unwrap_or_else(|| panic!("missed {line:?}"))
    }

    #[test]
    fn detects_desktop_framework_pack_and_normalizes_loopback() {
        let cases = [
            ("  ➜  Local: http://localhost:5173/", "vite", 5173),
            ("┃ Local    http://localhost:4321/", "vite", 4321),
            ("   - Local: http://localhost:3000", "next", 3000),
            ("* Listening on tcp://127.0.0.1:3001", "rails", 3001),
            (
                "Starting development server at http://0.0.0.0:8000/",
                "django",
                8000,
            ),
            (" * Running on http://127.0.0.1:5000", "flask", 5000),
            (
                "INFO: Uvicorn running on http://[::1]:8001",
                "uvicorn",
                8001,
            ),
            ("Serving HTTP on 0.0.0.0 port 9000", "http.server", 9000),
            ("Server listening on http://0.0.0.0:4000", "server", 4000),
        ];
        for (line, label, port) in cases {
            let match_ = found(line);
            assert_eq!(match_.label, label, "{line}");
            assert_eq!(match_.port, port, "{line}");
            assert!(match_.url.contains("localhost"), "{}", match_.url);
        }
    }

    #[test]
    fn keeps_portless_hosts_and_default_ports() {
        let https = found("  -> https://api.myapp.localhost");
        assert_eq!(https.url, "https://api.myapp.localhost");
        assert_eq!(https.port, 443);
        let http = found("  -> http://myapp.test");
        assert_eq!(http.url, "http://myapp.test");
        assert_eq!(http.port, 80);
    }

    #[test]
    fn detects_explicit_localdev_hostnames() {
        let server = found("https://gdn.localdev.nvidia.com:3852");
        assert_eq!(server.label, "server");
        assert_eq!(server.url, "https://gdn.localdev.nvidia.com:3852");
        assert_eq!(server.port, 3852);
    }

    #[test]
    fn rejects_commands_public_hosts_and_unrelated_urls() {
        for line in [
            "$ curl http://localhost:3000",
            "wget http://localhost:8080",
            "the server was running at http://localhost:3000",
            "Server listening on http://example.com:4000",
            "docs: http://localhost:3000",
        ] {
            assert_eq!(detect_server(line), None, "{line}");
        }
    }

    #[test]
    fn deduplicates_repeated_tail_ports() {
        let entries = detect_servers("Local: http://localhost:5173\nLocal: http://127.0.0.1:5173");
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn dual_stack_probe_requires_definitive_refusal() {
        assert_eq!(
            combine_connect_probes([ConnectProbe::Refused, ConnectProbe::Refused]),
            PortProbe::Refused
        );
        assert_eq!(
            combine_connect_probes([ConnectProbe::Refused, ConnectProbe::Unavailable]),
            PortProbe::Refused
        );
        assert_eq!(
            combine_connect_probes([ConnectProbe::Refused, ConnectProbe::Inconclusive]),
            PortProbe::Inconclusive
        );
        assert_eq!(
            combine_connect_probes([ConnectProbe::Inconclusive, ConnectProbe::Connected]),
            PortProbe::Listening
        );
        assert_eq!(
            combine_connect_probes([ConnectProbe::Refused, ConnectProbe::Connected]),
            PortProbe::Listening
        );
    }

    #[test]
    fn transient_probe_errors_cannot_accumulate_into_server_down() {
        let mut state = LivenessState::new();
        assert!(!state.observe(PortProbe::Refused));
        assert!(!state.observe(PortProbe::Refused));
        assert!(!state.observe(PortProbe::Inconclusive));
        assert!(!state.observe(PortProbe::Refused));
        assert!(!state.observe(PortProbe::Refused));
        assert!(state.observe(PortProbe::Refused));
    }
}
