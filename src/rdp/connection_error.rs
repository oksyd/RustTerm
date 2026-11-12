use super::RdpError;
use crate::i18n::{messages as key, tr};
use std::{fmt, io::ErrorKind};

/// A connection phase, without addresses, account names or protocol payloads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionStage {
    Resolve,
    Connect,
    SocketSetup,
    Negotiation,
    Tls,
    Authentication,
    SessionSetup,
    Session,
    Multitransport,
}

impl ConnectionStage {
    pub(super) fn code(self) -> &'static str {
        match self {
            Self::Resolve => "resolve",
            Self::Connect => "tcp-connect",
            Self::SocketSetup => "socket-setup",
            Self::Negotiation => "rdp-negotiation",
            Self::Tls => "tls-handshake",
            Self::Authentication => "authentication",
            Self::SessionSetup => "session-setup",
            Self::Session => "session-transfer",
            Self::Multitransport => "multitransport",
        }
    }
}

impl fmt::Display for ConnectionStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(tr(match self {
            Self::Resolve => key::RDP_STAGE_RESOLVE,
            Self::Connect => key::RDP_STAGE_CONNECT,
            Self::SocketSetup => key::RDP_STAGE_SOCKET_SETUP,
            Self::Negotiation => key::RDP_STAGE_NEGOTIATION,
            Self::Tls => key::RDP_STAGE_TLS,
            Self::Authentication => key::RDP_STAGE_AUTHENTICATION,
            Self::SessionSetup => key::RDP_STAGE_SESSION_SETUP,
            Self::Session => key::RDP_STAGE_SESSION,
            Self::Multitransport => key::RDP_STAGE_MULTITRANSPORT,
        }))
    }
}

/// Safe error categories: never retain an OS error's potentially sensitive text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionFailure {
    NameResolution,
    Refused,
    Reset,
    Closed,
    TimedOut,
    Unreachable,
    PermissionDenied,
    Other,
}

impl ConnectionFailure {
    pub(super) fn code(self) -> &'static str {
        match self {
            Self::NameResolution => "rdp-error-name-resolution",
            Self::Refused => "rdp-error-connection-refused",
            Self::Reset => "rdp-error-connection-reset",
            Self::Closed => "rdp-error-connection-closed",
            Self::TimedOut => "rdp-error-connection-timeout",
            Self::Unreachable => "rdp-error-network-unreachable",
            Self::PermissionDenied => "rdp-error-network-permission",
            Self::Other => "rdp-error-network-io",
        }
    }
}

impl From<ErrorKind> for ConnectionFailure {
    fn from(kind: ErrorKind) -> Self {
        match kind {
            ErrorKind::ConnectionRefused => Self::Refused,
            ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted => Self::Reset,
            ErrorKind::UnexpectedEof | ErrorKind::BrokenPipe | ErrorKind::NotConnected => {
                Self::Closed
            }
            ErrorKind::TimedOut => Self::TimedOut,
            ErrorKind::NetworkUnreachable
            | ErrorKind::HostUnreachable
            | ErrorKind::AddrNotAvailable => Self::Unreachable,
            ErrorKind::PermissionDenied => Self::PermissionDenied,
            _ => Self::Other,
        }
    }
}

impl fmt::Display for ConnectionFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(tr(match self {
            Self::NameResolution => key::RDP_FAILURE_NAME_RESOLUTION,
            Self::Refused => key::RDP_FAILURE_REFUSED,
            Self::Reset => key::RDP_FAILURE_RESET,
            Self::Closed => key::RDP_FAILURE_CLOSED,
            Self::TimedOut => key::RDP_FAILURE_TIMED_OUT,
            Self::Unreachable => key::RDP_FAILURE_UNREACHABLE,
            Self::PermissionDenied => key::RDP_FAILURE_PERMISSION_DENIED,
            Self::Other => key::RDP_FAILURE_OTHER,
        }))
    }
}

impl RdpError {
    pub(super) fn network(stage: ConnectionStage, failure: ConnectionFailure) -> Self {
        Self::Network { stage, failure }
    }

    pub(super) fn io(stage: ConnectionStage, error: std::io::Error) -> Self {
        Self::network(stage, error.kind().into())
    }

    pub(super) fn at_stage(self, stage: ConnectionStage) -> Self {
        match self {
            Self::Network { failure, .. } => Self::network(stage, failure),
            Self::Connection => Self::network(stage, ConnectionFailure::Other),
            Self::Timeout => Self::network(stage, ConnectionFailure::TimedOut),
            error => error,
        }
    }
}

// MS-RDPBCGR 2.2.1.2.2 and 2.2.5.1.1 use separate code namespaces.
// In particular, 5 means NLA required in negotiation and session takeover in
// Set Error Info. Never infer the namespace from the numeric value alone.
pub(super) fn negotiation_message(code: u32) -> &'static str {
    tr(match code {
        1 | 2 | 4 => key::RDP_SERVER_SECURITY_INCOMPATIBLE,
        3 => key::RDP_SERVER_CERTIFICATE_UNAVAILABLE,
        5 => key::RDP_RDP_REQUIRES_NETWORK_LEVEL_AUTHENTICATION_NLA,
        6 => key::RDP_SERVER_CLIENT_CERTIFICATE_REQUIRED,
        7 => key::RDP_SERVER_ENTRA_REQUIRED,
        _ => key::RDP_SERVER_NEGOTIATION_REJECTED,
    })
}

pub(super) fn disconnect_message(code: u32) -> &'static str {
    tr(match code {
        1 => key::RDP_SERVER_ADMIN_DISCONNECTED,
        2 => key::RDP_SERVER_ADMIN_LOGGED_OFF,
        3 => key::RDP_SERVER_IDLE_TIMEOUT,
        4 => key::RDP_SERVER_SESSION_TIME_LIMIT,
        5 => key::RDP_SERVER_SESSION_REPLACED,
        6 => key::RDP_SERVER_OUT_OF_MEMORY,
        7 => key::RDP_SERVER_CONNECTION_DENIED,
        _ => key::RDP_SERVER_SESSION_ENDED,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_server_codes_have_distinct_protocol_meanings() {
        let negotiation = RdpError::from(rustrdp::Error::NegotiationFailure(5));
        let disconnect = RdpError::from(rustrdp::Error::Disconnect(5));
        let generic = RdpError::from(rustrdp::Error::Server(5));
        assert_eq!(
            negotiation.to_string(),
            format!(
                "{} (0x00000005)",
                tr(key::RDP_RDP_REQUIRES_NETWORK_LEVEL_AUTHENTICATION_NLA)
            )
        );
        assert_eq!(
            disconnect.to_string(),
            format!("{} (0x00000005)", tr(key::RDP_SERVER_SESSION_REPLACED))
        );
        assert_ne!(generic.to_string(), disconnect.to_string());
        assert_ne!(negotiation.diagnostic_code(), disconnect.diagnostic_code());
        assert_eq!(negotiation.diagnostic_context(), Some("rdp-neg-failure"));
        assert_eq!(disconnect.diagnostic_context(), Some("set-error-info"));
        for error in [negotiation, disconnect, generic] {
            assert_eq!(error.server_code(), Some(5));
        }
        let unknown = RdpError::from(rustrdp::Error::Disconnect(0x1234_5678));
        assert_eq!(unknown.server_code(), Some(0x1234_5678));
        assert!(unknown.to_string().ends_with("(0x12345678)"));
        assert_eq!(RdpError::Authentication.server_code(), None);
    }

    #[test]
    fn network_errors_retain_safe_causes_and_never_expose_os_error_text() {
        for (kind, failure) in [
            (ErrorKind::ConnectionRefused, ConnectionFailure::Refused),
            (ErrorKind::ConnectionReset, ConnectionFailure::Reset),
            (ErrorKind::UnexpectedEof, ConnectionFailure::Closed),
            (ErrorKind::BrokenPipe, ConnectionFailure::Closed),
            (ErrorKind::TimedOut, ConnectionFailure::TimedOut),
            (
                ErrorKind::NetworkUnreachable,
                ConnectionFailure::Unreachable,
            ),
            (
                ErrorKind::PermissionDenied,
                ConnectionFailure::PermissionDenied,
            ),
            (ErrorKind::Other, ConnectionFailure::Other),
        ] {
            let error = RdpError::io(
                ConnectionStage::Authentication,
                std::io::Error::new(kind, "sensitive user@server.invalid/password"),
            );
            assert_eq!(
                error,
                RdpError::network(ConnectionStage::Authentication, failure)
            );
            assert_eq!(error.diagnostic_code(), failure.code());
            assert_eq!(error.diagnostic_context(), Some("authentication"));
            assert!(!format!("{error:?} {error}").contains("sensitive"));
            assert!(!error.to_string().contains("server.invalid"));
        }
        let protocol = RdpError::Protocol("invalid header");
        assert_eq!(
            protocol.clone().at_stage(ConnectionStage::Negotiation),
            protocol
        );
        assert_eq!(
            RdpError::Authentication.at_stage(ConnectionStage::Authentication),
            RdpError::Authentication
        );
    }
}
