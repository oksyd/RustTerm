use std::{future::Future, io, net::SocketAddr, time::Duration};
use tokio::net::TcpStream;

const FAMILY_DELAY: Duration = Duration::from_millis(250);

/// Respect the resolver's preferred family, but do not let a broken IPv6 or
/// IPv4 route consume the entire connection deadline before trying the other.
/// Each family keeps the resolver's order; at most two sockets are connecting.
pub(super) async fn connect(addresses: Vec<SocketAddr>) -> io::Result<TcpStream> {
    let prefer_v6 = addresses.first().is_some_and(SocketAddr::is_ipv6);
    let (preferred, fallback): (Vec<_>, Vec<_>) = addresses
        .into_iter()
        .partition(|address| address.is_ipv6() == prefer_v6);
    if fallback.is_empty() {
        return TcpStream::connect(preferred.as_slice()).await;
    }
    race_families(
        TcpStream::connect(preferred.as_slice()),
        TcpStream::connect(fallback.as_slice()),
    )
    .await
}

async fn race_families<T>(
    preferred: impl Future<Output = io::Result<T>>,
    fallback: impl Future<Output = io::Result<T>>,
) -> io::Result<T> {
    tokio::pin!(preferred, fallback);
    tokio::select! {
        biased;
        result = &mut preferred => return match result {
            Ok(stream) => Ok(stream),
            Err(_) => fallback.await,
        },
        _ = tokio::time::sleep(FAMILY_DELAY) => {}
    }
    // Dropping the losing future closes its pending socket. The caller wraps
    // this entire race in one deadline and cancellation scope.
    tokio::select! {
        biased;
        result = &mut preferred => match result {
            Ok(stream) => Ok(stream),
            Err(_) => fallback.await,
        },
        result = &mut fallback => match result {
            Ok(stream) => Ok(stream),
            Err(_) => preferred.await,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, future::pending};

    #[tokio::test]
    async fn stalled_preferred_family_does_not_block_fallback() {
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            race_families(pending::<io::Result<u8>>(), async { Ok(7) }),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result, 7);
    }

    #[tokio::test]
    async fn immediate_success_never_starts_fallback() {
        let started = Cell::new(false);
        let result = race_families(async { Ok(3) }, async {
            started.set(true);
            Ok(7)
        })
        .await
        .unwrap();
        assert_eq!(result, 3);
        assert!(!started.get());
    }

    #[tokio::test]
    async fn immediate_failure_skips_family_delay() {
        // A ready fallback must complete in the first poll, with no timer.
        use futures_util::FutureExt;
        let result = race_families(
            async { Err(io::Error::from(io::ErrorKind::ConnectionRefused)) },
            async { Ok(7) },
        )
        .now_or_never()
        .expect("fallback should start immediately")
        .unwrap();
        assert_eq!(result, 7);
    }
}
