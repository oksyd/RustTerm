use super::super::tests::{fixture, options};
use super::*;
use crate::config::ssh::{AutoForwardConfig, ServiceIdentity};

fn service(port: u16, owned: bool) -> DiscoveredService {
    DiscoveredService {
        identity: ServiceIdentity::Process {
            executable: "/usr/bin/node".into(),
            address: Ipv4Addr::LOCALHOST.into(),
            port,
        },
        name: "node".into(),
        project: None,
        host: "127.0.0.1".into(),
        port,
        owned,
        published: true,
    }
}
fn sample(services: Vec<DiscoveredService>) -> discovery::Sample {
    discovery::Sample {
        host: Some(services),
        docker: None,
        error: None,
        docker_error: None,
    }
}
fn manager() -> Manager {
    Manager::new(ForwardSettings {
        rules: Vec::new(),
        automatic: AutoForwardConfig {
            enabled: true,
            ..Default::default()
        },
    })
}

#[tokio::test]
async fn baseline_ownership_and_explicit_stop_survive_discovery_and_restart() {
    let (controller, _, connection, _) = fixture(&[]);
    let mut manager = manager();
    manager.apply_sample(sample(vec![service(3000, true)]), &controller);
    assert!(manager.settings.automatic.services.is_empty());
    manager.apply_sample(
        sample(vec![
            service(3000, true),
            service(4000, true),
            service(5000, false),
        ]),
        &controller,
    );
    assert_eq!(manager.settings.automatic.services.len(), 1);
    let id = manager.desired()[0].rule.id;
    assert_eq!(manager.desired()[0].rule.bind_host, "127.0.0.1");
    manager.enable(id, false);
    manager.apply_sample(sample(Vec::new()), &controller);
    manager.apply_sample(sample(vec![service(4000, true)]), &controller);
    assert!(!manager.desired()[0].enabled);
    let mut restarted = Manager::new(manager.settings.clone());
    assert_eq!(restarted.desired().len(), 1);
    restarted.apply_sample(sample(vec![service(4000, true)]), &controller);
    assert!(!restarted.desired()[0].enabled);
    connection.close().await;
}

#[tokio::test]
async fn url_hint_can_select_owned_service_in_first_snapshot_but_not_other_users() {
    let (controller, _, connection, _) = fixture(&[]);
    let mut manager = manager();
    let mut options = options(22, Vec::new());
    manager.control(
        Control::Hint(3000, ForwardProtocol::Http),
        &mut options,
        &controller,
    );
    manager.control(
        Control::Hint(4000, ForwardProtocol::Http),
        &mut options,
        &controller,
    );
    manager.apply_sample(
        sample(vec![service(3000, true), service(4000, false)]),
        &controller,
    );
    assert_eq!(manager.settings.automatic.services.len(), 1);
    assert_eq!(manager.desired()[0].rule.protocol, ForwardProtocol::Http);
    connection.close().await;
}

#[tokio::test]
async fn a_baseline_service_is_eligible_after_it_stops_and_is_started_again() {
    let (controller, _, connection, _) = fixture(&[]);
    let mut manager = manager();
    manager.apply_sample(sample(vec![service(3000, true)]), &controller);
    assert!(manager.desired().is_empty());
    manager.apply_sample(sample(Vec::new()), &controller);
    manager.apply_sample(sample(vec![service(3000, true)]), &controller);
    assert_eq!(manager.desired().len(), 1);
    connection.close().await;
}

#[tokio::test]
async fn failed_scan_preserves_state_and_restart_grace_starts_at_first_absence() {
    let (controller, _, connection, _) = fixture(&[]);
    let mut manager = manager();
    manager.apply_sample(sample(Vec::new()), &controller);
    manager.apply_sample(sample(vec![service(3000, true)]), &controller);
    manager.apply_sample(
        discovery::Sample {
            host: None,
            docker: Some(Vec::new()),
            error: Some("unavailable".into()),
            docker_error: None,
        },
        &controller,
    );
    assert!(manager.desired()[0].available);
    manager.catalog.values_mut().next().unwrap().seen = Instant::now() - Duration::from_secs(60);
    manager.apply_sample(sample(Vec::new()), &controller);
    let missing = &manager.desired()[0];
    assert!(
        missing.rule.enabled,
        "grace must start when absence is observed"
    );
    assert!(!missing.available);
    manager.catalog.values_mut().next().unwrap().missing_since =
        Some(Instant::now() - RESTART_GRACE);
    assert!(!manager.desired()[0].rule.enabled);
    assert!(
        manager.desired()[0].enabled,
        "saved preference stays enabled while waiting"
    );
    manager.apply_sample(sample(vec![service(3000, true)]), &controller);
    assert!(manager.desired()[0].available);
    assert!(manager.desired()[0].rule.enabled);
    connection.close().await;
}

#[tokio::test]
async fn compose_selection_skips_unpublished_ports_and_host_duplicates() {
    let (controller, _, connection, _) = fixture(&[]);
    let mut manager = manager();
    manager.settings.automatic.projects.push("project".into());
    let mut published = service(3000, false);
    published.identity = ServiceIdentity::Compose {
        project: "project".into(),
        service: "web".into(),
        replica: "1".into(),
        port: 80,
    };
    published.project = Some("project".into());
    let mut unpublished = published.clone();
    unpublished.identity = ServiceIdentity::Compose {
        project: "project".into(),
        service: "db".into(),
        replica: "1".into(),
        port: 5432,
    };
    unpublished.port = 5432;
    unpublished.published = false;
    manager.apply_sample(
        discovery::Sample {
            host: Some(vec![service(3000, true)]),
            docker: Some(vec![published.clone(), unpublished]),
            error: None,
            docker_error: None,
        },
        &controller,
    );
    assert_eq!(manager.settings.automatic.services.len(), 1);
    assert_eq!(controller.discovery().services.len(), 2);
    let id = manager.desired()[0].rule.id;
    published.port = 4000;
    manager.apply_sample(
        discovery::Sample {
            host: None,
            docker: Some(vec![published]),
            error: None,
            docker_error: None,
        },
        &controller,
    );
    assert_eq!(manager.desired()[0].rule.id, id);
    assert_eq!(manager.desired()[0].rule.destination_port, 4000);
    manager
        .settings
        .rules
        .push(manager.desired()[0].rule.clone());
    assert_eq!(manager.desired().len(), 1, "manual rule wins");
    connection.close().await;
}

#[tokio::test]
async fn automatic_conflict_chooses_free_port_and_reconciliation_keeps_listener() {
    let reserved = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = reserved.local_addr().unwrap().port();
    let (controller, _, connection, _) = fixture(&[]);
    let mut manager = manager();
    manager.apply_sample(sample(Vec::new()), &controller);
    manager.apply_sample(sample(vec![service(port, true)]), &controller);
    manager.reconcile(&controller, Some(&connection));
    let (index, generation, result) = manager
        .runtime
        .activations
        .join_next()
        .await
        .unwrap()
        .unwrap();
    manager.runtime.finish_start(
        index,
        generation,
        result,
        &manager.rules[index],
        &controller,
        &connection,
    );
    let address = controller.statuses()[0].local_address.unwrap();
    assert_ne!(address.port(), port);
    for _ in 0..3 {
        manager.reconcile(&controller, Some(&connection));
    }
    assert_eq!(controller.statuses()[0].local_address, Some(address));
    assert!(matches!(
        manager.runtime.rules[index].phase,
        RulePhase::Running(_)
    ));
    assert_eq!(manager.runtime.rules[index].generation, generation);
    assert_eq!(
        manager.settings.automatic.services[0].last_port,
        Some(address.port())
    );
    assert!(TcpListener::bind(address).await.is_err());
    manager.runtime.stop_all(&controller).await;
    assert!(TcpListener::bind(address).await.is_ok());
    connection.close().await;
}

#[tokio::test]
async fn explicit_fixed_port_never_falls_back_on_conflict() {
    let reserved = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = reserved.local_addr().unwrap().port();
    let rule = crate::config::ssh::parse_forwards(&format!("L 127.0.0.1:{port} 127.0.0.1:3000"))
        .unwrap()
        .remove(0);
    let (controller, _, connection, _) = fixture(std::slice::from_ref(&rule));
    let mut manager = Manager::new(ForwardSettings {
        rules: vec![rule],
        automatic: Default::default(),
    });
    manager.reconcile(&controller, Some(&connection));
    let (index, generation, result) = manager
        .runtime
        .activations
        .join_next()
        .await
        .unwrap()
        .unwrap();
    assert!(result.is_err());
    manager.runtime.finish_start(
        index,
        generation,
        result,
        &manager.rules[index],
        &controller,
        &connection,
    );
    manager.reconcile(&controller, Some(&connection));
    assert!(matches!(
        controller.statuses()[0].state,
        ForwardState::Failed(_)
    ));
    assert!(controller.statuses()[0].local_address.is_none());
    assert!(manager.runtime.activations.is_empty());
    connection.close().await;
}

#[tokio::test]
async fn pending_configuration_cannot_be_overwritten_by_an_older_actor_snapshot() {
    let (mut controller, _, connection, _) = fixture(&[]);
    let (send, mut receive) = mpsc::channel(8);
    controller.control = send;
    let mut manager = manager();
    let mut options = options(22, Vec::new());
    let mut first = manager.settings.clone();
    first.automatic.projects = vec!["one".into()];
    let mut second = first.clone();
    second.automatic.projects.push("two".into());
    controller.configure(first).unwrap();
    controller.configure(second.clone()).unwrap();
    manager.control(receive.recv().await.unwrap(), &mut options, &controller);
    assert_eq!(controller.settings(), second);
    manager.control(receive.recv().await.unwrap(), &mut options, &controller);
    assert_eq!(controller.settings(), second);
    connection.close().await;
}
