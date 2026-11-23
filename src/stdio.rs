use rmcp::RoleServer;
use rmcp::model::{
    ClientJsonRpcMessage, ClientNotification, ClientRequest, GetMeta, ProtocolVersion,
    ServerJsonRpcMessage,
};
use rmcp::transport::{Transport, async_rw::AsyncRwTransport};
use tokio::io::{AsyncRead, AsyncWrite};

pub fn transport(
    reader: impl AsyncRead + Send + Unpin + 'static,
    writer: impl AsyncWrite + Send + Unpin + 'static,
) -> impl Transport<RoleServer, Error = std::io::Error> {
    DiscoveryTransport {
        inner: AsyncRwTransport::new(reader, writer),
        probing: true,
    }
}

struct DiscoveryTransport<T> {
    inner: T,
    probing: bool,
}

impl<T: Transport<RoleServer>> Transport<RoleServer> for DiscoveryTransport<T> {
    type Error = T::Error;

    fn send(
        &mut self,
        message: ServerJsonRpcMessage,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        self.inner.send(message)
    }

    async fn receive(&mut self) -> Option<ClientJsonRpcMessage> {
        while self.probing {
            let message = self.inner.receive().await?;
            match &message {
                // rmcp answers each bootstrap probe before reading again. No
                // request is still running, even if its cancellation was queued.
                ClientJsonRpcMessage::Notification(n)
                    if matches!(n.notification, ClientNotification::CancelledNotification(_)) =>
                {
                    continue;
                }
                ClientJsonRpcMessage::Request(r) => {
                    self.probing = match &r.request {
                        ClientRequest::DiscoverRequest(_) | ClientRequest::PingRequest(_) => true,
                        ClientRequest::InitializeRequest(_) => false,
                        request => {
                            let meta = request.get_meta();
                            !meta
                                .missing_required_keys(&ProtocolVersion::NO_INITIALIZE)
                                .is_empty()
                                || !meta.protocol_version().is_some_and(|version| {
                                    ProtocolVersion::KNOWN_VERSIONS.contains(&version)
                                })
                        }
                    };
                }
                _ => {}
            }
            return Some(message);
        }
        self.inner.receive().await
    }

    fn close(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        self.inner.close()
    }
}
