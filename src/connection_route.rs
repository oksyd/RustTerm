//! Resolve persisted gateway references at the connection boundary. Workers only
//! receive an owned destination paired with a complete route; credentials and
//! destination configuration cannot subsequently drift apart.
use crate::config::{DatabaseConfig, ElasticsearchConfig, RdpConfig, RedisConfig, TelnetConfig};

pub(crate) enum Route<T> {
    Direct,
    Ssh(T),
}

pub(crate) struct Prepared<C, T> {
    destination: C,
    route: Route<T>,
}

pub(crate) trait Destination {
    fn requires_ssh(&self) -> bool;
}
macro_rules! destination {
    ($($config:ty),+ $(,)?) => {$ (
        impl Destination for $config {
            fn requires_ssh(&self) -> bool { self.ssh_profile.is_some() }
        }
    )+};
}
destination!(RdpConfig, RedisConfig, ElasticsearchConfig, TelnetConfig);
impl Destination for DatabaseConfig {
    fn requires_ssh(&self) -> bool {
        self.server()
            .is_some_and(|server| server.ssh_profile.is_some())
    }
}

#[derive(Debug)]
pub(crate) struct RouteMismatch;

impl<C: Destination, T> Prepared<C, T> {
    pub(crate) fn resolve(destination: C, ssh: Option<T>) -> Result<Self, RouteMismatch> {
        let route = match (destination.requires_ssh(), ssh) {
            (false, None) => Route::Direct,
            (true, Some(ssh)) => Route::Ssh(ssh),
            _ => return Err(RouteMismatch),
        };
        Ok(Self { destination, route })
    }
}
impl<T> Prepared<RedisConfig, T> {
    /// Explicit ephemeral routing, without a persisted gateway reference.
    pub(crate) fn via_ssh(destination: RedisConfig, ssh: T) -> Self {
        Self {
            destination,
            route: Route::Ssh(ssh),
        }
    }
}
impl<C, T> Prepared<C, T> {
    pub(crate) fn destination(&self) -> &C {
        &self.destination
    }
    pub(crate) fn into_parts(self) -> (C, Route<T>) {
        (self.destination, self.route)
    }
    /// Resolve credentials into a live tunnel without separating the route from
    /// its destination or rechecking the persisted profile reference.
    pub(crate) async fn try_map_ssh<U, E>(
        self,
        open: impl AsyncFnOnce(T) -> Result<U, E>,
    ) -> Result<Prepared<C, U>, E> {
        let route = match self.route {
            Route::Direct => Route::Direct,
            Route::Ssh(ssh) => Route::Ssh(open(ssh).await?),
        };
        Ok(Prepared {
            destination: self.destination,
            route,
        })
    }
}
impl<T> Route<T> {
    pub(crate) fn into_option(self) -> Option<T> {
        match self {
            Self::Direct => None,
            Self::Ssh(ssh) => Some(ssh),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{SqlConfig, SqliteConfig, Uuid};

    fn check<C: Destination>(direct: C, routed: C) {
        assert!(Prepared::<_, ()>::resolve(direct, Some(())).is_err());
        assert!(Prepared::<_, ()>::resolve(routed, None).is_err());
    }
    #[test]
    fn route_mismatches_are_rejected_at_the_boundary_for_every_remote_protocol() {
        let id = Some(Uuid::new_v4());
        check(
            RdpConfig::default(),
            RdpConfig {
                ssh_profile: id,
                ..Default::default()
            },
        );
        check(
            TelnetConfig::default(),
            TelnetConfig {
                ssh_profile: id,
                ..Default::default()
            },
        );
        check(
            RedisConfig::default(),
            RedisConfig {
                ssh_profile: id,
                ..Default::default()
            },
        );
        check(
            ElasticsearchConfig::default(),
            ElasticsearchConfig {
                ssh_profile: id,
                ..Default::default()
            },
        );
        for engine in [
            DatabaseConfig::Mysql,
            DatabaseConfig::Postgres,
            DatabaseConfig::ClickHouse,
        ] {
            check(
                engine(SqlConfig::mysql()),
                engine(SqlConfig {
                    ssh_profile: id,
                    ..SqlConfig::mysql()
                }),
            );
        }
        assert!(
            Prepared::resolve(DatabaseConfig::Sqlite(SqliteConfig::default()), Some(())).is_err()
        );
    }
    #[tokio::test]
    async fn resolving_transport_keeps_the_destination_and_never_opens_a_direct_route() {
        let direct = Prepared::<_, ()>::resolve(RdpConfig::default(), None).unwrap();
        let direct = direct
            .try_map_ssh(async |_| -> Result<(), ()> { panic!("direct route opened SSH") })
            .await
            .unwrap();
        assert!(matches!(direct.into_parts().1, Route::Direct));
        let config = RdpConfig {
            host: "remote.test".into(),
            ssh_profile: Some(Uuid::new_v4()),
            ..Default::default()
        };
        let connection = Prepared::resolve(config, Some("credentials")).unwrap();
        let (destination, route) = connection
            .try_map_ssh(async |credentials| {
                assert_eq!(credentials, "credentials");
                Ok::<_, ()>("tunnel")
            })
            .await
            .unwrap()
            .into_parts();
        assert_eq!(destination.host, "remote.test");
        assert!(matches!(route, Route::Ssh("tunnel")));
    }
}
