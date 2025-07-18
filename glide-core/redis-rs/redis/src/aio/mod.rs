//! Adds async IO support to redis.
use crate::cmd::{cmd, Cmd};
use crate::connection::{
    get_resp3_hello_command_error, PubSubSubscriptionKind, RedisConnectionInfo,
};
use crate::pipeline::PipelineRetryStrategy;
use crate::types::{
    ErrorKind, FromRedisValue, InfoDict, ProtocolVersion, RedisError, RedisFuture, RedisResult,
    Value,
};
use crate::PushKind;
use ::tokio::io::{AsyncRead, AsyncWrite};
use async_trait::async_trait;
use futures_util::Future;
use std::net::SocketAddr;
#[cfg(unix)]
use std::path::Path;
use std::pin::Pin;
use std::time::Duration;

use crate::tls::TlsConnParams;

/// Enables the tokio compatibility
#[cfg(feature = "tokio-comp")]
#[cfg_attr(docsrs, doc(cfg(feature = "tokio-comp")))]
pub mod tokio;

/// Represents the ability of connecting via TCP or via Unix socket
#[async_trait]
pub(crate) trait RedisRuntime: AsyncStream + Send + Sync + Sized + 'static {
    /// Performs a TCP connection
    async fn connect_tcp(socket_addr: SocketAddr) -> RedisResult<Self>;

    // Performs a TCP TLS connection
    async fn connect_tcp_tls(
        hostname: &str,
        socket_addr: SocketAddr,
        insecure: bool,
        tls_params: &Option<TlsConnParams>,
    ) -> RedisResult<Self>;

    /// Performs a UNIX connection
    #[cfg(unix)]
    async fn connect_unix(path: &Path) -> RedisResult<Self>;

    fn spawn(f: impl Future<Output = ()> + Send + 'static);

    fn boxed(self) -> Pin<Box<dyn AsyncStream + Send + Sync>> {
        Box::pin(self)
    }
}

/// Trait for objects that implements `AsyncRead` and `AsyncWrite`
pub trait AsyncStream: AsyncRead + AsyncWrite {}
impl<S> AsyncStream for S where S: AsyncRead + AsyncWrite {}

/// An async abstraction over connections.
pub trait ConnectionLike {
    /// Sends an already encoded (packed) command into the TCP socket and
    /// reads the single response from it.
    fn req_packed_command<'a>(&'a mut self, cmd: &'a Cmd) -> RedisFuture<'a, Value>;

    /// Sends multiple already encoded (packed) command into the TCP socket
    /// and reads `count` responses from it.  This is used to implement
    /// pipelining.
    /// Important - this function is meant for internal usage, since it's
    /// easy to pass incorrect `offset` & `count` parameters, which might
    /// cause the connection to enter an erroneous state. Users shouldn't
    /// call it, instead using the Pipeline::query_async function.
    #[doc(hidden)]
    fn req_packed_commands<'a>(
        &'a mut self,
        cmd: &'a crate::Pipeline,
        offset: usize,
        count: usize,
        pipeline_retry_strategy: Option<PipelineRetryStrategy>,
    ) -> RedisFuture<'a, Vec<Value>>;

    /// Returns the database this connection is bound to.  Note that this
    /// information might be unreliable because it's initially cached and
    /// also might be incorrect if the connection like object is not
    /// actually connected.
    fn get_db(&self) -> i64;

    /// Returns the state of the connection
    fn is_closed(&self) -> bool;

    /// Get the connection availibility zone
    fn get_az(&self) -> Option<String> {
        None
    }

    /// Set the connection availibility zone
    fn set_az(&mut self, _az: Option<String>) {}
}

/// Implements ability to notify about disconnection events
#[async_trait]
pub trait DisconnectNotifier: Send + Sync {
    /// Notify about disconnect event
    fn notify_disconnect(&mut self);

    /// Wait for disconnect event with timeout
    async fn wait_for_disconnect_with_timeout(&self, max_wait: &Duration);

    /// Intended to be used with Box
    fn clone_box(&self) -> Box<dyn DisconnectNotifier>;
}

impl Clone for Box<dyn DisconnectNotifier> {
    fn clone(&self) -> Box<dyn DisconnectNotifier> {
        self.clone_box()
    }
}

// Helper function to extract and update availability zone from INFO command
async fn update_az_from_info<C>(con: &mut C) -> RedisResult<()>
where
    C: ConnectionLike,
{
    let info_res = con.req_packed_command(&cmd("INFO")).await;

    match info_res {
        Ok(value) => {
            let info_dict: InfoDict = FromRedisValue::from_redis_value(&value)?;
            if let Some(node_az) = info_dict.get::<String>("availability_zone") {
                con.set_az(Some(node_az));
            }
            Ok(())
        }
        Err(e) => {
            // Handle the error case for the INFO command
            Err(RedisError::from((
                ErrorKind::ResponseError,
                "Failed to execute INFO command. ",
                format!("{e:?}"),
            )))
        }
    }
}

// Initial setup for every connection.
async fn setup_connection<C>(
    connection_info: &RedisConnectionInfo,
    con: &mut C,
    // This parameter is set to 'true' if ReadFromReplica strategy is set to AZAffinity or AZAffinityReplicasAndPrimary.
    // An INFO command will be triggered in the connection's setup to update the 'availability_zone' property.
    discover_az: bool,
) -> RedisResult<()>
where
    C: ConnectionLike,
{
    if connection_info.protocol != ProtocolVersion::RESP2 {
        let hello_cmd = resp3_hello(connection_info);
        let val: RedisResult<Value> = hello_cmd.query_async(con).await;
        if let Err(err) = val {
            return Err(get_resp3_hello_command_error(err));
        }
    } else if let Some(password) = &connection_info.password {
        let mut command = cmd("AUTH");
        if let Some(username) = &connection_info.username {
            command.arg(username);
        }
        match command.arg(password).query_async(con).await {
            Ok(Value::Okay) => (),
            Err(e) => {
                let err_msg = e.detail().ok_or((
                    ErrorKind::AuthenticationFailed,
                    "Password authentication failed",
                ))?;

                if !err_msg.contains("wrong number of arguments for 'auth' command") {
                    fail!((
                        ErrorKind::AuthenticationFailed,
                        "Password authentication failed",
                    ));
                }

                let mut command = cmd("AUTH");
                match command.arg(password).query_async(con).await {
                    Ok(Value::Okay) => (),
                    _ => {
                        fail!((
                            ErrorKind::AuthenticationFailed,
                            "Password authentication failed"
                        ));
                    }
                }
            }
            _ => {
                fail!((
                    ErrorKind::AuthenticationFailed,
                    "Password authentication failed"
                ));
            }
        }
    }

    if connection_info.db != 0 {
        match cmd("SELECT").arg(connection_info.db).query_async(con).await {
            Ok(Value::Okay) => (),
            _ => fail!((
                ErrorKind::ResponseError,
                "Redis server refused to switch database"
            )),
        }
    }

    if let Some(client_name) = &connection_info.client_name {
        match cmd("CLIENT")
            .arg("SETNAME")
            .arg(client_name)
            .query_async(con)
            .await
        {
            Ok(Value::Okay) => {}
            _ => fail!((
                ErrorKind::ResponseError,
                "Redis server refused to set client name"
            )),
        }
    }

    if discover_az {
        update_az_from_info(con).await?;
    }

    // result is ignored, as per the command's instructions.
    // https://redis.io/commands/client-setinfo/
    let _: RedisResult<()> = crate::connection::client_set_info_pipeline()
        .query_async(con)
        .await;

    // resubscribe
    if connection_info.protocol != ProtocolVersion::RESP3 {
        return Ok(());
    }
    static KIND_TO_COMMAND: [(PubSubSubscriptionKind, &str); 3] = [
        (PubSubSubscriptionKind::Exact, "SUBSCRIBE"),
        (PubSubSubscriptionKind::Pattern, "PSUBSCRIBE"),
        (PubSubSubscriptionKind::Sharded, "SSUBSCRIBE"),
    ];

    if connection_info.pubsub_subscriptions.is_none() {
        return Ok(());
    }

    for (subscription_kind, channels_patterns) in
        connection_info.pubsub_subscriptions.as_ref().unwrap()
    {
        for channel_pattern in channels_patterns.iter() {
            let mut subscribe_command =
                cmd(KIND_TO_COMMAND[Into::<usize>::into(*subscription_kind)].1);
            subscribe_command.arg(channel_pattern);

            // This is a quite intricate code - Per RESP3, subscriptions commands do not return anything.
            // Instead, push messages will be pushed for each channel. Thus, this is not a typycal request-response pattern.
            // The act of pushing is asyncronous with the regard to the subscription command, and might be delayed for some time after the server state was already updated.
            // (i.e. the behaviour is implementation defined).
            // We will assume the configured time out is enough for the server to push the notifications.
            match subscribe_command.query_async(con).await {
                Ok(Value::Push { kind, data }) => {
                    match *subscription_kind {
                        PubSubSubscriptionKind::Exact => {
                            if kind != PushKind::Subscribe
                                || Value::BulkString(channel_pattern.clone()) != data[0]
                            {
                                fail!((
                                    ErrorKind::ResponseError,
                                    // TODO: Consider printing the exact command
                                    "Failed to restore Exact subscription channels"
                                ));
                            }
                        }
                        PubSubSubscriptionKind::Pattern => {
                            if kind != PushKind::PSubscribe
                                || Value::BulkString(channel_pattern.clone()) != data[0]
                            {
                                fail!((
                                    ErrorKind::ResponseError,
                                    // TODO: Consider printing the exact command
                                    "Failed to restore Pattern subscription channels"
                                ));
                            }
                        }
                        PubSubSubscriptionKind::Sharded => {
                            if kind != PushKind::SSubscribe
                                || Value::BulkString(channel_pattern.clone()) != data[0]
                            {
                                fail!((
                                    ErrorKind::ResponseError,
                                    // TODO: Consider printing the exact command
                                    "Failed to restore Sharded subscription channels"
                                ));
                            }
                        }
                    }
                }
                _ => {
                    fail!((
                        ErrorKind::ResponseError,
                        // TODO: Consider printing the exact command
                        "Failed to receive subscription notification while restoring subscription channels"
                    ));
                }
            }
        }
    }

    Ok(())
}

mod connection;
pub use connection::*;
mod multiplexed_connection;
pub use multiplexed_connection::*;
#[cfg(feature = "connection-manager")]
mod connection_manager;
#[cfg(feature = "connection-manager")]
#[cfg_attr(docsrs, doc(cfg(feature = "connection-manager")))]
pub use connection_manager::*;
mod runtime;
use crate::commands::resp3_hello;
pub(super) use runtime::*;
