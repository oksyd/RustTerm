//! SQLx's private Unix-socket bridge is unavailable on Windows. Never replace
//! it with an unprotected loopback listener or weaken database TLS verification.
use super::{DatabaseError, Result};
use crate::{
    config::{SqlConfig, SqlEngine},
    session::TunnelConnector,
};
use std::path::Path;
use tokio_util::sync::CancellationToken;
pub(super) struct TunnelBridge;
impl TunnelBridge {
    pub async fn open(
        _: SqlEngine,
        _: &SqlConfig,
        _: TunnelConnector,
        _: CancellationToken,
    ) -> Result<Self> {
        Err(DatabaseError::SshTunnelPlatformUnavailable)
    }
    pub fn directory(&self) -> &Path {
        unreachable!("Windows SQL tunnel construction is rejected")
    }
    pub fn socket(&self) -> &Path {
        unreachable!("Windows SQL tunnel construction is rejected")
    }
}
