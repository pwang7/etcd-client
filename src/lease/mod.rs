//! Leases are a mechanism for detecting client liveness. The cluster grants leases with a time-to-live. A lease expires if the etcd cluster does not receive a keepAlive within a given TTL period.
//!
//! # Examples
//!
//! Grant lease and keep lease alive
//!
//! ```no_run
//! use std::time::Duration;
//!
//! use etcd_rs::*;
//!
//! #[tokio::main]
//! async fn main() -> Result<()> {
//!     let client = Client::connect(ClientConfig {
//!         endpoints: vec!["http://127.0.0.1:2379".to_owned()],
//!         auth: None,
//!         tls: None,
//!     }).await?;
//!
//!     let key = "foo";
//!
//!     // grant lease
//!     let lease = client
//!         .lease()
//!         .grant(LeaseGrantRequest::new(Duration::from_secs(3)))
//!         .await?;
//!
//!     let lease_id = lease.id();
//!
//!     // set key with lease
//!     client
//!         .kv()
//!         .put({
//!             let mut req = PutRequest::new(key, "bar");
//!             req.set_lease(lease_id);
//!
//!             req
//!         })
//!         .await?;
//!
//!     {
//!         // keep alive the lease every 1 second
//!         let client = client.clone();
//!
//!         let mut interval = tokio::time::interval(Duration::from_secs(1));
//!
//!         loop {
//!             interval.tick().await;
//!             client
//!                 .lease()
//!                 .keep_alive(LeaseKeepAliveRequest::new(lease_id))
//!                 .await;
//!         }
//!     }
//!
//!     // not necessary, but will cleanly shut down the long-running tasks
//!     // spawned by the client
//!     client.shutdown().await;
//!
//!     Ok(())
//! }
//! ```

use std::sync::Arc;

use async_trait::async_trait;
use futures::future::FutureExt;
use tokio::stream::Stream;
use tokio::sync::{
    mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
    oneshot,
};
use tonic::transport::Channel;

pub use grant::{LeaseGrantRequest, LeaseGrantResponse};
pub use keep_alive::{LeaseKeepAliveRequest, LeaseKeepAliveResponse};
pub use revoke::{LeaseRevokeRequest, LeaseRevokeResponse};

use crate::lazy::{Lazy, Shutdown};
use crate::proto::etcdserverpb;
use crate::proto::etcdserverpb::lease_client::LeaseClient;
use crate::{Error, Result};

mod grant;
mod keep_alive;
mod revoke;

/// LeaseKeepAliveTunnel is a reusable connection for `Lease Keep Alive` operation.
/// The underlying gRPC method is Bi-directional streaming.
struct LeaseKeepAliveTunnel {
    req_sender: UnboundedSender<LeaseKeepAliveRequest>,
    resp_receiver: Option<UnboundedReceiver<Result<LeaseKeepAliveResponse>>>,
    shutdown: Option<oneshot::Sender<()>>,
}

impl LeaseKeepAliveTunnel {
    fn new(mut client: LeaseClient<Channel>) -> Self {
        let (req_sender, mut req_receiver) = unbounded_channel::<LeaseKeepAliveRequest>();
        let (resp_sender, resp_receiver) = unbounded_channel::<Result<LeaseKeepAliveResponse>>();

        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        let request = tonic::Request::new(async_stream::stream! {
            while let Some(req) = req_receiver.recv().await {
                let pb: etcdserverpb::LeaseKeepAliveRequest = req.into();
                yield pb;
            }
        });

        // monitor inbound watch response and transfer to the receiver
        tokio::spawn(async move {
            let mut shutdown_rx = shutdown_rx.fuse();
            let mut inbound = futures::select! {
                res = client.lease_keep_alive(request).fuse() => res.unwrap().into_inner(),
                _ = shutdown_rx => { return; }
            };

            loop {
                let resp = futures::select! {
                    resp = inbound.message().fuse() => resp,
                    _ = shutdown_rx => { return; }
                };
                match resp {
                    Ok(Some(resp)) => {
                        resp_sender.send(Ok(From::from(resp))).unwrap();
                    }
                    Ok(None) => {
                        return;
                    }
                    Err(e) => {
                        resp_sender.send(Err(From::from(e))).unwrap();
                    }
                };
            }
        });

        Self {
            req_sender,
            resp_receiver: Some(resp_receiver),
            shutdown: Some(shutdown_tx),
        }
    }

    fn take_resp_receiver(&mut self) -> UnboundedReceiver<Result<LeaseKeepAliveResponse>> {
        self.resp_receiver.take().unwrap()
    }
}

#[async_trait]
impl Shutdown for LeaseKeepAliveTunnel {
    async fn shutdown(&mut self) -> Result<()> {
        if let Some(shutdown) = self.shutdown.take() {
            shutdown.send(()).map_err(|_| Error::ChannelClosed)?;
        }
        Ok(())
    }
}

/// Lease client.
#[derive(Clone)]
pub struct Lease {
    client: LeaseClient<Channel>,
    keep_alive_tunnel: Arc<Lazy<LeaseKeepAliveTunnel>>,
}

impl Lease {
    pub(crate) fn new(client: LeaseClient<Channel>) -> Self {
        let keep_alive_tunnel = {
            let client = client.clone();
            Arc::new(Lazy::new(move || LeaseKeepAliveTunnel::new(client.clone())))
        };
        Self {
            client,
            keep_alive_tunnel,
        }
    }

    /// Performs a lease granting operation.
    pub async fn grant(&mut self, req: LeaseGrantRequest) -> Result<LeaseGrantResponse> {
        let resp = self
            .client
            .lease_grant(tonic::Request::new(req.into()))
            .await?;

        Ok(From::from(resp.into_inner()))
    }

    /// Performs a lease revoking operation.
    pub async fn revoke(&mut self, req: LeaseRevokeRequest) -> Result<LeaseRevokeResponse> {
        let resp = self
            .client
            .lease_revoke(tonic::Request::new(req.into()))
            .await?;

        Ok(From::from(resp.into_inner()))
    }

    /// Fetch keep alive response stream.
    pub async fn keep_alive_responses(
        &mut self,
    ) -> impl Stream<Item = Result<LeaseKeepAliveResponse>> {
        self.keep_alive_tunnel.write().await.take_resp_receiver()
    }

    /// Performs a lease refreshing operation.
    pub async fn keep_alive(&mut self, req: LeaseKeepAliveRequest) -> Result<()> {
        self.keep_alive_tunnel
            .write()
            .await
            .req_sender
            .send(req)
            .map_err(|_| Error::ChannelClosed)
    }

    /// Shut down the running lease task, if any.
    pub async fn shutdown(&mut self) -> Result<()> {
        // If we implemented `Shutdown` for this, callers would need it in scope in
        // order to call this method.
        self.keep_alive_tunnel.evict().await
    }
}