use std::{net::SocketAddr, sync::Arc, time::Duration};

use barbirolli::Barbirolli;
use russh::{
    Channel, ChannelOpenFailure,
    keys::{Certificate, ssh_key},
    server::{self, Msg, Server, Session},
};
use tokio::{io::copy_bidirectional, net::TcpListener};

pub struct Meier {
    manager: Arc<Barbirolli>,
    authenticated_user: Option<String>,
}

impl Meier {
    pub fn new(manager: Arc<Barbirolli>) -> Self {
        Self {
            manager,
            authenticated_user: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MeierError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Russh(#[from] russh::Error),
}

pub async fn serve(manager: Arc<Barbirolli>, address: SocketAddr) -> Result<(), MeierError> {
    let config = russh::server::Config {
        inactivity_timeout: Some(Duration::from_secs(3600)),
        auth_rejection_time: Duration::from_secs(3),
        auth_rejection_time_initial: Some(Duration::ZERO),
        keys: vec![
            russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
                .expect("Ed25519 host-key generation should succeed"),
        ],
        ..Default::default()
    };
    let listener = TcpListener::bind(address).await?;
    Meier::new(manager)
        .run_on_socket(Arc::new(config), &listener)
        .await?;
    Ok(())
}

impl Server for Meier {
    type Handler = Self;

    fn new_client(&mut self, _: Option<SocketAddr>) -> Self {
        Self::new(self.manager.clone())
    }

    fn handle_session_error(&mut self, error: <Self::Handler as server::Handler>::Error) {
        tracing::error!(%error, "SSH connection failed");
    }
}

impl server::Handler for Meier {
    type Error = MeierError;

    async fn auth_publickey(
        &mut self,
        user: &str,
        key: &ssh_key::PublicKey,
    ) -> Result<server::Auth, Self::Error> {
        let Some(path) = self.manager.authorized_keys_path(user).await else {
            return Ok(server::Auth::reject());
        };

        if cfg!(feature = "local") {
            self.authenticated_user = Some(user.to_owned());
            return Ok(server::Auth::Accept);
        }

        let authorized_keys = match tokio::fs::read_to_string(path).await {
            Ok(authorized_keys) => authorized_keys,
            Err(error) => {
                tracing::error!(user, %error, "failed to read VM authorized keys");
                return Ok(server::Auth::reject());
            }
        };
        let authorized = key_is_authorized(&authorized_keys, key);
        if authorized {
            self.authenticated_user = Some(user.to_owned());
            Ok(server::Auth::Accept)
        } else {
            Ok(server::Auth::reject())
        }
    }

    async fn auth_openssh_certificate(
        &mut self,
        user: &str,
        _certificate: &Certificate,
    ) -> Result<server::Auth, Self::Error> {
        if cfg!(feature = "local") && self.manager.authorized_keys_path(user).await.is_some() {
            self.authenticated_user = Some(user.to_owned());
            Ok(server::Auth::Accept)
        } else {
            Ok(server::Auth::reject())
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<Msg>,
        _host_to_connect: &str,
        port_to_connect: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        let Some(user) = self.authenticated_user.as_deref() else {
            reply
                .reject(ChannelOpenFailure::AdministrativelyProhibited)
                .await;
            return Ok(());
        };
        if port_to_connect != 22 {
            reply
                .reject(ChannelOpenFailure::AdministrativelyProhibited)
                .await;
            return Ok(());
        }
        let Some(address) = self.manager.ssh_address(user).await else {
            reply.reject(ChannelOpenFailure::ConnectFailed).await;
            return Ok(());
        };
        let socket = match tokio::net::TcpStream::connect((address, 22)).await {
            Ok(socket) => socket,
            Err(error) => {
                tracing::warn!(user, %address, %error, "failed to connect to VM SSH service");
                reply.reject(ChannelOpenFailure::ConnectFailed).await;
                return Ok(());
            }
        };

        reply.accept().await;
        tokio::spawn(async move {
            let mut channel = channel.into_stream();
            let mut socket = socket;
            if let Err(error) = copy_bidirectional(&mut channel, &mut socket).await {
                tracing::warn!(%error, "VM SSH forwarding ended with an error");
            }
        });
        Ok(())
    }
}

fn key_is_authorized(authorized_keys: &str, key: &ssh_key::PublicKey) -> bool {
    authorized_keys
        .lines()
        .filter_map(|line| ssh_key::PublicKey::from_openssh(line).ok())
        .any(|candidate| candidate.key_data() == key.key_data())
}

#[cfg(test)]
mod tests {
    use super::key_is_authorized;

    #[test]
    fn authorized_keys_are_compared_as_complete_openssh_keys() {
        let private =
            russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
                .unwrap();
        let other =
            russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
                .unwrap();
        let authorized = format!(
            "# comment\n{} owner\n",
            private.public_key().to_openssh().unwrap()
        );

        assert!(key_is_authorized(&authorized, private.public_key()));
        assert!(!key_is_authorized(&authorized, other.public_key()));
    }
}
