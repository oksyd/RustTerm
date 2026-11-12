use super::*;
use crate::config::ssh::ForwardProtocol;
use serde_json::json;

#[test]
fn listener_snapshot_verifies_owner_and_excludes_both_ssh_ports() {
    let text = "RUSTTERM_PORTS_V1 1000\n\
LISTEN 0 128 0.0.0.0:3000 0.0.0.0:* users:((\"node\",pid=42,fd=4)) uid:1000\n\
LISTEN 0 128 [::1]:8080 [::]:* users:((\"python\",pid=43,fd=4))\n\
LISTEN 0 128 127.0.0.1:9000 0.0.0.0:* users:((\"other\",pid=44,fd=4))\n\
LISTEN 0 128 *:2222 *:*\n\
LISTEN 0 128 *:22 *:*\n\
LISTEN 0 128 [fe80::1%eth0]:4000 [::]:*\n\
RUSTTERM_PORTS_END\n\
RUSTTERM_PROCESS\t42\t1000\t/usr/bin/node\n\
RUSTTERM_PROCESS\t43\t1000\t/usr/bin/python3\n\
RUSTTERM_PROCESS\t44\t1001\t/usr/bin/other\n\
RUSTTERM_SSH_PORT 2222\n";
    let found = parse_host(text, 22).unwrap();
    assert_eq!(found.len(), 3);
    let node = found.iter().find(|s| s.port == 3000).unwrap();
    assert!(node.owned);
    assert_eq!(node.host, "127.0.0.1");
    assert_eq!(node.name, "node");
    assert_eq!(found.iter().find(|s| s.port == 8080).unwrap().host, "::1");
    assert!(!found.iter().find(|s| s.port == 9000).unwrap().owned);
    assert!(parse_host("RUSTTERM_PORTS_V1 1000\nLISTEN 0 1 *:3000 *:*", 22).is_err());
    assert!(parse_host("command not found", 22).is_err());
}

#[test]
fn docker_uses_published_tcp_ports_and_stable_compose_identity() {
    let container = |id: &str, state: &str, replica: &str| {
        serde_json::from_value(json!({
        "Id": id, "Names": ["/project-web-1"], "Image": "example", "State": state, "Status": "running",
        "Labels": {"com.docker.compose.project":"project", "com.docker.compose.service":"web", "com.docker.compose.container-number":replica},
        "Ports": [
            {"IP":"::", "PrivatePort":80, "PublicPort":3000, "Type":"tcp"},
            {"IP":"0.0.0.0", "PrivatePort":80, "PublicPort":3000, "Type":"tcp"},
            {"PrivatePort":9000, "Type":"tcp"},
            {"IP":"0.0.0.0", "PrivatePort":53, "PublicPort":5353, "Type":"udp"}
        ]
    })).unwrap()
    };
    let first = docker_services(vec![
        container("old", "running", "1"),
        container("stopped", "exited", "2"),
    ])
    .unwrap();
    assert_eq!(first.len(), 2);
    let published = first.iter().find(|s| s.published).unwrap();
    assert_eq!(
        (published.host.as_str(), published.port),
        ("127.0.0.1", 3000)
    );
    assert!(
        matches!(&published.identity, ServiceIdentity::Compose { port:80, replica, .. } if replica == "1")
    );
    let second = docker_services(vec![container("new", "running", "1")]).unwrap();
    assert_eq!(first, second);
    assert!(!first.iter().find(|s| s.port == 9000).unwrap().published);
}

#[test]
fn url_hints_handle_split_ports_and_only_accept_loopback_addresses() {
    let mut hints = OutputHints::default();
    assert!(hints.push(b"ready https://localhost:30").is_empty());
    assert_eq!(hints.push(b"00/\n"), [(3000, ForwardProtocol::Https)]);
    let mut hints = OutputHints::default();
    assert!(
        hints
            .push(b"http://example.com:3000/ http://127.0.0.1:65536/ http://localhost:0/\n")
            .is_empty()
    );
    assert_eq!(
        hints.push(b"http://[::1]:8080/\n"),
        [(8080, ForwardProtocol::Http)]
    );
    assert!(hints.push(b"http://localhost:9000/\n").is_empty());
    hints.push(&vec![b'x'; 100_000]);
    assert!(hints.tail.len() <= 4096);
}

#[test]
fn excessive_listener_sets_are_errors_instead_of_partial_snapshots() {
    let mut snapshot = "RUSTTERM_PORTS_V1 1000\n".to_owned();
    for port in 10000..10257 {
        snapshot.push_str(&format!("LISTEN 0 1 *:{port} *:*\n"));
    }
    snapshot.push_str("RUSTTERM_PORTS_END\n");
    assert!(parse_host(&snapshot, 22).is_err());
}
