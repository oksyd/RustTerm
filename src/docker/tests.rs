use super::*;
use serde_json::json;
#[test]
fn negotiates_an_intersection_instead_of_trusting_the_server_version() {
    for (max, min, expected) in [
        ("1.55", "1.40", Some("1.47")),
        ("1.44", "1.24", Some("1.44")),
        ("1.40", "1.24", None),
        ("1.55", "1.48", None),
        ("1.44", "1.46", None),
        ("garbage", "1.24", None),
    ] {
        let mut version = EngineVersion {
            version: "test".into(),
            api: max.into(),
            minimum_api: min.into(),
            os: "linux".into(),
        };
        let result = version.negotiate();
        if let Some(api) = expected {
            result.unwrap();
            assert_eq!(version.api, api);
        } else {
            assert!(result.is_err());
        }
    }
}
#[test]
fn socket_paths_are_shell_quoted_and_cannot_select_a_tcp_context() {
    assert!(Endpoint::parse("tcp://host:2375").is_err());
    assert!(Endpoint::parse("/tmp/socket\ncommand").is_err());
    let endpoint = Endpoint::parse("/tmp/it's $(echo bad).sock").unwrap();
    assert!(
        endpoint
            .command()
            .contains("'unix:///tmp/it'\\''s $(echo bad).sock'")
    );
    assert!(
        Endpoint::System
            .command()
            .contains("--host 'unix:///var/run/docker.sock'")
    );
    for id in ["name", "../id?x=1", "", &"a".repeat(63), &"x".repeat(64)] {
        assert!(validate_id(id).is_err());
    }
}
#[test]
fn log_multiplexing_handles_every_split_boundary_and_incomplete_frames() {
    let frame = [
        &[1, 0, 0, 0, 0, 0, 0, 6][..],
        "你好".as_bytes(),
        &[2, 0, 0, 0, 0, 0, 0, 4],
        b"err\n",
    ]
    .concat();
    for split in 0..=frame.len() {
        let mut decoder = logs::Decoder::new(false);
        let mut parts = decoder.push(&frame[..split]).unwrap();
        parts.extend(decoder.push(&frame[split..]).unwrap());
        decoder.finish().unwrap();
        let out: Vec<_> = parts
            .iter()
            .filter(|(err, _)| !*err)
            .flat_map(|(_, bytes)| bytes.clone())
            .collect();
        let err: Vec<_> = parts
            .iter()
            .filter(|(err, _)| *err)
            .flat_map(|(_, bytes)| bytes.clone())
            .collect();
        assert_eq!(out, "你好".as_bytes());
        assert_eq!(err, b"err\n");
    }
    let mut decoder = logs::Decoder::new(false);
    decoder.push(&frame[..3]).unwrap();
    assert!(decoder.finish().is_err());
    assert!(
        logs::Decoder::new(false)
            .push(&[1, 0, 0, 0, 0xff, 0xff, 0xff, 0xff])
            .is_err()
    );
    assert!(
        logs::Decoder::new(false)
            .push(&[1, 1, 0, 0, 0, 0, 0, 0])
            .is_err()
    );
    assert_eq!(
        logs::Decoder::new(true).push(b"raw\0text").unwrap()[0].1,
        b"raw\0text"
    );
}
#[test]
fn log_buffer_preserves_unicode_across_chunks_and_bounds_long_lines() {
    let mut logs = LogBuffer::default();
    for &byte in "中文\n\n".as_bytes() {
        logs.append(false, &[byte]);
    }
    assert_eq!(logs.text(""), "中文\n");
    for _ in 0..3000 {
        logs.append(true, b"error\n");
    }
    assert_eq!(logs.lines().count(), 2000);
    assert_eq!(logs.evicted, 1002);
    logs.append(false, &vec![b'a'; 2 * 1024 * 1024]);
    logs.finish();
    assert!(logs.text("").len() <= 1024 * 1024 + 2000);
    assert!(logs.lines().all(|line| line.text.len() <= 16 * 1024));
}
#[test]
fn linux_stats_use_cpu_deltas_and_cgroup_cache_not_raw_memory() {
    let before:Stats=decode(&serde_json::to_vec(&json!({"cpu_stats":{"cpu_usage":{"total_usage":100},"system_cpu_usage":1000,"online_cpus":4}})).unwrap()).unwrap();
    let after:Stats=decode(&serde_json::to_vec(&json!({"cpu_stats":{"cpu_usage":{"total_usage":125},"system_cpu_usage":1100,"online_cpus":4},"memory_stats":{"usage":1024,"limit":4096,"stats":{"inactive_file":256}}})).unwrap()).unwrap();
    assert_eq!(after.cpu_percent(&before), Some(100.));
    assert_eq!(before.cpu_percent(&after), None);
    assert_eq!(after.memory_usage(), Some(768));
}

#[test]
fn empty_daemon_collections_accept_json_null() {
    let container: Container = decode(br#"{"Id":"abc","Names":null,"Image":"scratch","State":"exited","Status":"Exited (0)","Labels":null,"Ports":null}"#).unwrap();
    assert!(container.labels.is_empty());
    assert!(container.ports.is_empty());
    assert_eq!(container.project(), "");
    let stats: Stats =
        decode(br#"{"networks":null,"memory_stats":{"usage":1024,"stats":null}}"#).unwrap();
    assert_eq!(stats.network_bytes(), (0, 0));
    assert_eq!(stats.memory_usage(), Some(1024));
}
