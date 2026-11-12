//! Independent TCP probes and opt-in remote desktop idle prevention.
//! RDP's Server Heartbeat PDU is server-to-client, so it is not a client ping.
use super::{ConnectionStage, RdpCommand, RdpError, Result};
use socket2::{SockRef, TcpKeepalive};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::time::Instant;

const DESKTOP_IDLE_INTERVAL: Duration = Duration::from_secs(30);

pub(super) struct DesktopActivity {
    deadline: Option<Instant>,
}

impl DesktopActivity {
    pub fn input_at(&mut self, now: Instant) {
        if let Some(deadline) = self.deadline {
            self.deadline = Some(deadline.max(now + DESKTOP_IDLE_INTERVAL));
        }
    }
    pub fn new(enabled: bool, now: Instant) -> Self {
        Self {
            deadline: enabled.then_some(now + DESKTOP_IDLE_INTERVAL),
        }
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    pub fn observe(&mut self, command: &RdpCommand, now: Instant) {
        if self.deadline.is_some()
            && matches!(
                command,
                RdpCommand::Key { .. }
                    | RdpCommand::Unicode(_)
                    | RdpCommand::Pointer { .. }
                    | RdpCommand::MouseButton { .. }
                    | RdpCommand::Wheel { .. }
                    | RdpCommand::PasteRemote
                    | RdpCommand::ReleaseAll
                    | RdpCommand::SetLocked(_)
            )
        {
            self.deadline = Some(now + DESKTOP_IDLE_INTERVAL);
        }
    }

    pub fn poll(&mut self, now: Instant, active: bool, locked: bool) -> bool {
        if self.deadline.is_none_or(|deadline| deadline > now) {
            return false;
        }
        // No catch-up bursts after suspension; also advance while reactivating
        // or locked so an expired deadline cannot spin the connection loop.
        self.deadline = Some(now + DESKTOP_IDLE_INTERVAL);
        active && !locked
    }
}

pub(super) fn configure(stream: &TcpStream, seconds: u64) -> Result<()> {
    if seconds > 3600 {
        return Err(RdpError::InvalidConfig);
    }
    let socket = SockRef::from(stream);
    if seconds == 0 {
        return socket
            .set_keepalive(false)
            .map_err(|error| RdpError::io(ConnectionStage::SocketSetup, error));
    }
    let interval = Duration::from_secs(seconds);
    socket
        .set_tcp_keepalive(
            &TcpKeepalive::new()
                .with_time(interval)
                .with_interval(interval)
                .with_retries(5),
        )
        .map_err(|error| RdpError::io(ConnectionStage::SocketSetup, error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    #[test]
    fn desktop_activity_is_opt_in_and_tracks_input_not_network_traffic() {
        let now = Instant::now();
        let input = RdpCommand::Pointer { x: 12, y: 34 };
        let mut off = DesktopActivity::new(false, now);
        off.observe(&input, now);
        assert_eq!(off.deadline(), None);
        assert!(!off.poll(now + Duration::from_secs(600), true, false));

        let mut activity = DesktopActivity::new(true, now);
        activity.observe(&input, now + Duration::from_secs(20));
        activity.observe(
            &RdpCommand::Resize {
                width: 800,
                height: 600,
            },
            now + Duration::from_secs(45),
        );
        assert_eq!(activity.deadline(), Some(now + Duration::from_secs(50)));
        assert!(!activity.poll(now + Duration::from_secs(49), true, false));
        assert!(activity.poll(now + Duration::from_secs(50), true, false));
        assert!(!activity.poll(now + Duration::from_secs(50), true, false));
    }

    #[test]
    fn locked_or_inactive_desktops_skip_activity_without_spinning_or_catch_up() {
        let now = Instant::now();
        let mut activity = DesktopActivity::new(true, now);
        assert!(!activity.poll(now + Duration::from_secs(30), false, false));
        assert_eq!(activity.deadline(), Some(now + Duration::from_secs(60)));
        assert!(!activity.poll(now + Duration::from_secs(60), true, true));
        assert_eq!(activity.deadline(), Some(now + Duration::from_secs(90)));
        activity.observe(&RdpCommand::SetLocked(false), now + Duration::from_secs(80));
        assert!(!activity.poll(now + Duration::from_secs(90), true, false));
        assert!(activity.poll(now + Duration::from_secs(600), true, false));
        assert_eq!(activity.deadline(), Some(now + Duration::from_secs(630)));
        assert!(!activity.poll(now + Duration::from_secs(600), true, false));
    }

    #[tokio::test]
    async fn keepalive_is_applied_to_the_socket_and_can_be_disabled() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (mut server, _) = listener.accept().await.unwrap();
        for seconds in [30, 1, 3600] {
            configure(&client, seconds).unwrap();
            let socket = SockRef::from(&client);
            assert!(socket.keepalive().unwrap());
            #[cfg(target_os = "linux")]
            {
                assert_eq!(
                    socket.tcp_keepalive_time().unwrap(),
                    Duration::from_secs(seconds)
                );
                assert_eq!(
                    socket.tcp_keepalive_interval().unwrap(),
                    Duration::from_secs(seconds)
                );
                assert_eq!(socket.tcp_keepalive_retries().unwrap(), 5);
            }
        }
        assert_eq!(configure(&client, 3601), Err(RdpError::InvalidConfig));
        configure(&client, 0).unwrap();
        assert!(!SockRef::from(&client).keepalive().unwrap());

        // Configuring keepalive must not consume the stream or inject RDP/input
        // bytes into it; normal traffic still reaches the peer unchanged.
        client.write_all(b"rdp").await.unwrap();
        let mut received = [0; 3];
        server.read_exact(&mut received).await.unwrap();
        assert_eq!(&received, b"rdp");
    }
}
