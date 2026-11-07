//! Opt-in interoperability check using a saved profile, credential and certificate pin.
//! Set RUSTTERM_TEST_RDP_HOST and optionally RUSTTERM_TEST_RDP_QUALITY, then run
//! `cargo test --test rdp -- --ignored --nocapture`. No remote input is sent.
use rustterm::{
    config::{AppConfig, Connection, CredentialStore, RdpQuality, SystemCredentialStore},
    rdp::{CertificateDecision, RdpClient, RdpEvent},
};

#[test]
#[ignore = "requires an explicitly selected saved RDP profile and unlocked credential store"]
fn saved_rdp_desktop() {
    let host = std::env::var("RUSTTERM_TEST_RDP_HOST").expect("target required");
    let path = rustterm::config::config_dir().unwrap().join("config.toml");
    let app = AppConfig::load(path).unwrap();
    let mut config = app
        .profiles
        .into_iter()
        .find_map(|p| match p.connection {
            Connection::Rdp(c) if c.host == host => Some(c),
            _ => None,
        })
        .expect("saved RDP profile");
    assert!(config.certificate_sha256.is_some(), "saved trust required");
    let password = SystemCredentialStore::new()
        .get(config.credential.unwrap())
        .unwrap();
    config.clipboard = false;
    config.prevent_idle = false;
    config.clipboard_files = false;
    config.audio_output = false;
    config.audio_input = false;
    config.shared_folders.clear();
    if let Ok(quality) = std::env::var("RUSTTERM_TEST_RDP_QUALITY") {
        config.apply_quality(match quality.as_str() {
            "clear" => RdpQuality::Clear,
            "smooth" => RdpQuality::Smooth,
            "bandwidth" => RdpQuality::Bandwidth,
            _ => panic!("quality must be clear, smooth or bandwidth"),
        });
    }
    let mut handle = RdpClient::connect(config, password).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(45);
    let mut connected = false;
    let mut decoded_and_acknowledged = false;
    let mut first_desktop = None;
    let sender = handle.sender();
    while std::time::Instant::now() < deadline {
        if let Some(frame) = handle.frames.borrow_and_update().as_ref() {
            if let Some(token) = &frame.acknowledgement {
                sender.frame_presented(token.clone());
            }
            if frame.tiles.iter().any(|tile| {
                tile.pixels
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .any(|p| p[..3] != [0; 3])
            }) {
                first_desktop.get_or_insert_with(std::time::Instant::now);
            }
        }
        while let Ok(event) = handle.events.try_recv() {
            match event {
                RdpEvent::Connecting => {
                    connected = false;
                    first_desktop = None;
                    decoded_and_acknowledged = false;
                }
                RdpEvent::Connected => {
                    connected = true;
                    eprintln!("desktop activated");
                }
                RdpEvent::CertificateChallenge { reply, .. } => {
                    let _ = reply.send(CertificateDecision::Reject);
                    handle.close();
                    panic!("certificate differs from saved trust");
                }
                RdpEvent::Failed(error) => {
                    handle.close();
                    panic!("RDP failed: {error:?}");
                }
                RdpEvent::Disconnected => panic!("RDP disconnected"),
                RdpEvent::Performance(p) => {
                    decoded_and_acknowledged |= p.completed_frames > 0 && p.acknowledged_frames > 0;
                    eprintln!(
                        "graphics: codec={:?}, completed={}, acknowledged={}",
                        p.graphics_codec, p.completed_frames, p.acknowledged_frames
                    );
                }
                _ => (),
            }
        }
        if connected
            && decoded_and_acknowledged
            && first_desktop.is_some_and(|t| t.elapsed().as_secs() >= 6)
        {
            eprintln!("desktop pixels received, connection remained active");
            handle.close();
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    handle.close();
    panic!("no desktop frame; connected={connected}");
}
