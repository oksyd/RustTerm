//! Drive the public API with a small synthetic post-authentication transcript.
//!
//! From the crate directory, run `cargo run --example replay`. No network, desktop or
//! credentials are accessed. In a real client, TLS verification and CredSSP
//! authentication must finish before `Connector::start`, and every outbound
//! packet below must be written in order to that same authenticated transport.

use rustrdp::{ClientConfig, Connector, Error, Event, Input, SecurityProtocol, Session, wire};

const SERVER: &[u8] = include_bytes!("fixtures/server.rdp");

fn main() -> rustrdp::Result<()> {
    let config = ClientConfig {
        username: "offline-example".into(),
        width: 320,
        height: 200,
        ..ClientConfig::default()
    };
    let max_pdu = config.limits.max_pdu;
    let mut connector = Connector::new(config, SecurityProtocol::Hybrid)?;
    let mut sent_bytes = connector.start()?.len();
    let mut session: Option<Session> = None;
    let mut received_frames = 0;
    let mut disconnected = false;
    let mut buffered = Vec::new();

    // Tiny chunks deliberately split both length headers and payloads. Network
    // reads can also combine several frames, so drain every complete frame.
    'stream: for chunk in SERVER.chunks(7) {
        buffered.extend_from_slice(chunk);
        while let Some((_, length)) = wire::frame_size(&buffered, max_pdu)? {
            if buffered.len() < length {
                break;
            }
            let frame = &buffered[..length];
            received_frames += 1;
            let (events, activated) = if let Some(session) = &mut session {
                (session.process(frame)?, false)
            } else {
                let output = connector.advance(frame)?;
                sent_bytes += output.packets.iter().map(Vec::len).sum::<usize>();
                let activated = output.session.is_some();
                session = output.session;
                (output.events, activated)
            };
            for event in events {
                match event {
                    Event::Send(packet) => sent_bytes += packet.len(),
                    Event::GraphicsChanged
                    | Event::GraphicsFrameBegin { .. }
                    | Event::GraphicsFrameEnd { .. } => {
                        // Coalesce early notifications until a Session exists.
                        // A UI can now schedule a redraw of session.graphics().
                    }
                    Event::Resized { width, height } => println!("Desktop is {width} x {height}"),
                    Event::ClipboardOffer
                    | Event::ClipboardText(_)
                    | Event::ClipboardContent(_)
                    | Event::ClipboardUnavailable => {
                        // A real application applies its explicit clipboard policy.
                    }
                    Event::ClipboardPublished | Event::ClipboardPublishRejected => {
                        // A real application completes or cancels a local paste request.
                    }
                    Event::ClipboardFileRequest(_)
                    | Event::ClipboardFilesReleased { .. }
                    | Event::ClipboardFiles { .. }
                    | Event::ClipboardFileContents { .. }
                    | Event::ClipboardRemoteFilesCancelled { .. }
                    | Event::Audio(_)
                    | Event::Capture(_)
                    | Event::DriveRequest(_) => {
                        // This headless replay does not enable files, audio or shared drives.
                    }
                    Event::RequestRejected(error) => eprintln!("Local request rejected: {error}"),
                    Event::MultitransportRequest(_)
                    | Event::SendTunnel { .. }
                    | Event::MultitransportSynchronized { .. } => {
                        return Err(Error::Unsupported(
                            "multitransport is disabled in this replay",
                        ));
                    }
                    Event::Disconnected { reason } => {
                        println!("Server disconnected (reason {reason})");
                        disconnected = true;
                        drop(session.take());
                    }
                }
            }
            buffered.drain(..length);
            if disconnected {
                // A live driver closes the transport here and reads no more data.
                break 'stream;
            }
            if activated && let Some(established) = &mut session {
                let graphics = established.graphics();
                println!(
                    "Activated {} x {} desktop ({} BGRA bytes); local pointer hidden: {}",
                    graphics.width(),
                    graphics.height(),
                    graphics.pixels().len(),
                    graphics.hide_local_pointer(),
                );
                // The first frame already contains the bitmap and pointer
                // received before activation. Publish it before new local input.
                for input in [
                    Input::Key {
                        scancode: 0x1e,
                        pressed: true,
                    },
                    Input::ReleaseAll,
                ] {
                    for event in established.input(input)? {
                        if let Event::Send(packet) = event {
                            sent_bytes += packet.len();
                        }
                    }
                }
            }
        }
    }
    if !disconnected && !buffered.is_empty() {
        return Err(Error::Truncated);
    }
    if !disconnected {
        return Err(Error::Invalid("incomplete offline transcript"));
    }
    println!("Replayed {received_frames} server frames; produced {sent_bytes} outbound bytes");
    Ok(())
}
