//! Redis implementation for wasmcloud:keyvalue.
//!
//! This implementation is multi-threaded and operations between different actors
//! use different connections and can run in parallel.
//! A single connection is shared by all instances of the same actor id (public key),
//! so there may be some brief lock contention if several instances of the same actor
//! are simultaneously attempting to communicate with redis. See documentation
//! on the [exec](#exec) function for more information.
//!
//!
use redis::{aio::Connection, FromRedisValue, RedisError};
use std::{collections::HashMap, convert::Infallible, ops::DerefMut, sync::Arc};
use tokio::sync::RwLock;
use wasmbus_rpc::provider::prelude::*;
use wasmcloud_interface_keyvalue::{
    GetResponse, IncrementRequest, KeyValue, KeyValueReceiver, ListAddRequest, ListDelRequest,
    ListRangeRequest, SetAddRequest, SetDelRequest, SetRequest, StringList,
};

const REDIS_URL_KEY: &str = "URL";
const DEFAULT_CONNECT_URL: &str = "redis://0.0.0.0:6379/";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // handle lattice control messages and forward rpc to the provider dispatch
    // returns when provider receives a shutdown control message
    provider_main(KvRedisProvider::default())?;

    eprintln!("KVRedis provider exiting");
    Ok(())
}

/// Redis keyValue provider implementation.
#[derive(Default, Clone, Provider)]
#[services(KeyValue)]
struct KvRedisProvider {
    // store redis connections per actor
    actors: Arc<RwLock<HashMap<String, RwLock<Connection>>>>,
}
/// use default implementations of provider message handlers
impl ProviderDispatch for KvRedisProvider {}

/// Handle provider control commands
/// put_link (new actor link command), del_link (remove link command), and shutdown
#[async_trait]
impl ProviderHandler for KvRedisProvider {
    /// Provider should perform any operations needed for a new link,
    /// including setting up per-actor resources, and checking authorization.
    /// If the link is allowed, return true, otherwise return false to deny the link.
    async fn put_link(&self, ld: &LinkDefinition) -> RpcResult<bool> {
        let redis_url = match ld.values.get(REDIS_URL_KEY) {
            Some(v) => v.as_str(),
            None => DEFAULT_CONNECT_URL,
        };
        let client = redis::Client::open(redis_url).map_err(|e| {
            RpcError::ProviderInit(format!("redis connection to {}: {}", redis_url, e))
        })?;
        let connection = client.get_async_connection().await.map_err(|e| {
            RpcError::ProviderInit(format!("redis connection to {}: {}", redis_url, e))
        })?;

        let mut update_map = self.actors.write().await;
        update_map.insert(ld.actor_id.to_string(), RwLock::new(connection));

        Ok(true)
    }

    /// Handle notification that a link is dropped - close the connection
    async fn delete_link(&self, actor_id: &str) {
        let mut aw = self.actors.write().await;
        if let Some(conn) = aw.remove(actor_id) {
            log::info!("redis closing connection for actor {}", actor_id);
            drop(conn)
        }
    }

    /// Handle shutdown request by closing all connections
    async fn shutdown(&self) -> Result<(), Infallible> {
        let mut aw = self.actors.write().await;
        // empty the actor link data and stop all servers
        for (_, conn) in aw.drain() {
            drop(conn)
        }
        Ok(())
    }
}

fn to_rpc_err(e: RedisError) -> RpcError {
    RpcError::Other(format!("redis error: {}", e))
}

// There are two api styles you can use for invoking redis. You can build any raw command
// as a string command and a sequence of args:
// ```
//     let mut cmd = redis::cmd("SREM");
//     let value: u32 = self.exec(ctx, &mut cmd.arg(&arg.set_name).arg(&arg.value)).await?;
// ```
// or you can call a method on Cmd, as in
// ```
//     let mut cmd = redis::Cmd::srem(&arg.set_name, &arg.value);
//     let value: u32 = self.exec(ctx, &mut cmd).await?;
//```
// The latter api style has better rust compile-time type checking for args.
// The rust docs for cmd and Cmd don't document arg types or return types.
// For that, you need to look at https://redis.io/commands#

/// Handle KeyValue methods that interact with redis
#[async_trait]
impl KeyValue for KvRedisProvider {
    /// Increments a numeric value, returning the new value
    async fn increment(&self, ctx: &Context, arg: &IncrementRequest) -> RpcResult<i32> {
        let mut cmd = redis::Cmd::incr(&arg.key, &arg.value);
        let val: i32 = self.exec(ctx, &mut cmd).await?;
        Ok(val)
    }

    /// Returns true if the store contains the key
    async fn contains<TS: ToString + ?Sized + Sync>(
        &self,
        ctx: &Context,
        arg: &TS,
    ) -> RpcResult<bool> {
        let mut cmd = redis::Cmd::exists(arg.to_string());
        let val: bool = self.exec(ctx, &mut cmd).await?;
        Ok(val)
    }

    /// Deletes a key, returning true if the key was deleted
    async fn del<TS: ToString + ?Sized + Sync>(&self, ctx: &Context, arg: &TS) -> RpcResult<bool> {
        let mut cmd = redis::Cmd::del(arg.to_string());
        let val: i32 = self.exec(ctx, &mut cmd).await?;
        Ok(val > 0)
    }

    /// Gets a value for a specified key. If the key exists,
    /// the return structure contains exists: true and the value,
    /// otherwise the return structure contains exists == false.
    async fn get<TS: ToString + ?Sized + Sync>(
        &self,
        ctx: &Context,
        arg: &TS,
    ) -> RpcResult<GetResponse> {
        let mut cmd = redis::Cmd::get(arg.to_string());
        let val: Option<String> = self.exec(ctx, &mut cmd).await?;
        let resp = match val {
            Some(s) => GetResponse {
                exists: true,
                value: s,
            },
            None => GetResponse {
                exists: false,
                ..Default::default()
            },
        };
        Ok(resp)
    }

    /// Append a value onto the end of a list. Returns the new list size
    async fn list_add(&self, ctx: &Context, arg: &ListAddRequest) -> RpcResult<u32> {
        let mut cmd = redis::Cmd::rpush(&arg.list_name, &arg.value);
        let val: u32 = self.exec(ctx, &mut cmd).await?;
        Ok(val)
    }

    /// Deletes a list and its contents
    /// input: list name
    /// returns: true if the list existed and was deleted
    async fn list_clear<TS: ToString + ?Sized + Sync>(
        &self,
        ctx: &Context,
        arg: &TS,
    ) -> RpcResult<bool> {
        self.del(ctx, arg).await
    }

    /// Deletes an item from a list. Returns true if the item was removed.
    async fn list_del(&self, ctx: &Context, arg: &ListDelRequest) -> RpcResult<bool> {
        let mut cmd = redis::Cmd::lrem(&arg.list_name, 1, &arg.value);
        let val: u32 = self.exec(ctx, &mut cmd).await?;
        Ok(val > 0)
    }

    /// Retrieves a range of values from a list using 0-based indices.
    /// Start and end values are inclusive, for example, (0,10) returns
    /// 11 items if the list contains at least 11 items. If the stop value
    /// is beyond the end of the list, it is treated as the end of the list.
    async fn list_range(&self, ctx: &Context, arg: &ListRangeRequest) -> RpcResult<StringList> {
        let mut cmd = redis::Cmd::lrange(&arg.list_name, arg.start as isize, arg.stop as isize);
        let val: StringList = self.exec(ctx, &mut cmd).await?;
        Ok(val)
    }

    /// Sets the value of a key.
    /// expires is an optional number of seconds before the value should be automatically deleted,
    /// or 0 for no expiration.
    async fn set(&self, ctx: &Context, arg: &SetRequest) -> RpcResult<()> {
        let mut cmd = redis::Cmd::set(&arg.key, &arg.value);
        let _value: Option<String> = self.exec(ctx, &mut cmd).await?;
        Ok(())
    }

    /// Add an item into a set. Returns number of items added
    async fn set_add(&self, ctx: &Context, arg: &SetAddRequest) -> RpcResult<u32> {
        let mut cmd = redis::Cmd::sadd(&arg.set_name, &arg.value);
        let value: u32 = self.exec(ctx, &mut cmd).await?;
        Ok(value)
    }

    /// Remove a item from the set. Returns
    async fn set_del(&self, ctx: &Context, arg: &SetDelRequest) -> RpcResult<u32> {
        let mut cmd = redis::Cmd::srem(&arg.set_name, &arg.value);
        let value: u32 = self.exec(ctx, &mut cmd).await?;
        Ok(value)
    }

    /// Deletes a set and its contents
    /// input: set name
    /// returns: true if the set existed and was deleted
    async fn set_clear<TS: ToString + ?Sized + Sync>(
        &self,
        ctx: &Context,
        arg: &TS,
    ) -> RpcResult<bool> {
        self.del(ctx, arg).await
    }

    async fn set_intersection(
        &self,
        ctx: &Context,
        arg: &StringList,
    ) -> Result<StringList, RpcError> {
        let mut cmd = redis::Cmd::sinter(arg);
        let value: Vec<String> = self.exec(ctx, &mut cmd).await?;
        Ok(value)
    }

    async fn set_query<TS: ToString + ?Sized + Sync>(
        &self,
        ctx: &Context,
        arg: &TS,
    ) -> RpcResult<StringList> {
        let mut cmd = redis::Cmd::smembers(arg.to_string());
        let values: Vec<String> = self.exec(ctx, &mut cmd).await?;
        Ok(values)
    }

    async fn set_union(&self, ctx: &Context, arg: &StringList) -> RpcResult<StringList> {
        let mut cmd = redis::Cmd::sunion(arg);
        let values: Vec<String> = self.exec(ctx, &mut cmd).await?;
        Ok(values)
    }
}

impl KvRedisProvider {
    /// Helper function to execute redis async command while holding onto a mutable connection.
    ///
    /// This provider is multi-threaded, and requests from different actors use
    /// different connections, and requests can run in parallel.
    ///
    /// There is a single connection per actor public key, and the write lock on the connection
    /// effectively serializes redis operations for all instances of the same actor.
    /// The lock is held only for the duration of a redis command from this provider
    /// and waiting for its response. The lock duration does not overlap with
    /// message passing between actors and this provider, including serialization
    /// of requests and deserialization of responses, which are fully parallelizable.
    ///
    /// There is a read lock held on the actors hashtable, which does not interfere
    /// with redis operations, but any control commands for new actor links
    /// or removal of actor links may need to wait for in-progress operations to complete.
    /// That should be rare, because most links are passed to the provider at startup.
    async fn exec<T: FromRedisValue>(&self, ctx: &Context, cmd: &mut redis::Cmd) -> RpcResult<T> {
        let actor_id = ctx
            .actor
            .as_ref()
            .ok_or_else(|| RpcError::InvalidParameter("no actor in request".to_string()))?;
        // get read lock on actor-connections hashmap
        let rd = self.actors.read().await;
        let rc = rd
            .get(actor_id)
            .ok_or_else(|| RpcError::InvalidParameter(format!("actor not linked:{}", actor_id)))?;
        // get write lock on this actor's connection
        let mut con = rc.write().await;
        cmd.query_async(con.deref_mut()).await.map_err(to_rpc_err)
    }
}
