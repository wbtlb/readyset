use anyhow::{anyhow, bail, Error};
use async_trait::async_trait;
use consul::kv::{KVPair, KV};
use consul::session::{Session, SessionEntry};
use consul::Config;
use reqwest::ClientBuilder;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use super::WorkerId;
use super::{
    AuthorityControl, AuthorityWorkerHeartbeatResponse, GetLeaderResult, LeaderPayload,
    WorkerDescriptor,
};
use crate::errors::internal_err;
use crate::{ReadySetError, ReadySetResult};

pub const WORKER_PREFIX: &str = "workers/";
/// Path to the leader key.
pub const CONTROLLER_KEY: &str = "controller";
/// Path to the controller state.
pub const STATE_KEY: &str = "state";
/// The delay before another client can claim a lock.
const SESSION_LOCK_DELAY: u64 = 0;
/// When the authority releases locks on keys, the keys should
/// be deleted.
const SESSION_RELEASE_BEHAVIOR: &str = "delete";
/// The amount of time to wait for a heartbeat before declaring a
/// session as dead.
const SESSION_TTL: &str = "20s";

struct ConsulAuthorityInner {
    session: Option<SessionEntry>,
    /// The last index that the controller key was modified or
    /// created at.
    controller_index: Option<u64>,
}

/// Coordinator that shares connection information between workers and clients using Consul.
pub struct ConsulAuthority {
    /// The consul client.
    consul: consul::Client,

    /// Deployment associated with this authority.
    deployment: String,

    /// Internal authority state required to handle operations.
    inner: Option<RwLock<ConsulAuthorityInner>>,
}

fn path_to_worker_id(path: &str) -> WorkerId {
    // See `worker_id_to_prefix` for the type of path this is called on.
    #[allow(clippy::unwrap_used)]
    path[(path.rfind('/').unwrap() + 1)..].to_owned()
}

fn worker_id_to_path(id: &str) -> String {
    WORKER_PREFIX.to_owned() + id
}

impl ConsulAuthority {
    /// The connect string should be in the format of
    /// http(s)://<address>:<port>/<deployment>.
    fn new_with_inner(
        connect_string: &str,
        inner: Option<RwLock<ConsulAuthorityInner>>,
    ) -> ReadySetResult<Self> {
        // We artificially create a namespace for each deployment by prefixing the
        // deployment to each keys path.
        let split_idx = connect_string.rfind('/').ok_or_else(|| {
            ReadySetError::Internal("Consul connect string missing deployment".to_owned())
        })?;

        let deployment = connect_string[(split_idx + 1)..].to_owned();
        let address = connect_string[..split_idx].to_owned();

        let config = ClientBuilder::new()
            .build()
            .map(|client| Config {
                address,
                datacenter: None,
                http_client: client,
                token: None,
                wait_time: None,
            })
            .map_err(|e| ReadySetError::Internal(e.to_string()))?;

        let consul = consul::Client::new(config);

        let authority = Self {
            consul,
            deployment,
            inner,
        };

        Ok(authority)
    }

    /// Create a new instance.
    pub fn new(connect_string: &str) -> ReadySetResult<Self> {
        let inner = Some(RwLock::new(ConsulAuthorityInner {
            controller_index: None,
            session: None,
        }));
        Self::new_with_inner(connect_string, inner)
    }

    async fn create_session(&self) -> Result<(), Error> {
        let session = {
            let inner = self.read_inner()?;
            inner.session.clone()
        };

        if session.is_none() {
            match self
                .consul
                .create(
                    &SessionEntry {
                        // All keys attached to this session are ephemeral keys.
                        Behavior: Some(SESSION_RELEASE_BEHAVIOR.to_string()),
                        // Disable lock delaying so a new leader can claim the lock
                        // immediately when it is relinquished.
                        LockDelay: Some(SESSION_LOCK_DELAY),
                        // The amount of time to wait for a heartbeat before declaring
                        // a ession dead.
                        TTL: Some(SESSION_TTL.to_string()),
                        ..Default::default()
                    },
                    None,
                )
                .await
            {
                Ok((entry, _)) => {
                    let mut inner = self.write_inner()?;
                    inner.session = Some(entry);
                }
                Err(e) => bail!(e.to_string()),
            }
        };

        Ok(())
    }

    fn get_session(&self) -> Result<String, Error> {
        let inner = self.read_inner()?;
        // Both fields are guarenteed to be populated previously or
        // above.
        #[allow(clippy::unwrap_used)]
        Ok(inner.session.as_ref().unwrap().ID.clone().unwrap())
    }

    fn read_inner(&self) -> Result<RwLockReadGuard<'_, ConsulAuthorityInner>, Error> {
        if let Some(inner_mutex) = &self.inner {
            match inner_mutex.read() {
                Ok(inner) => Ok(inner),
                Err(e) => bail!(internal_err(format!("rwlock is poisoned: '{}'", e))),
            }
        } else {
            bail!(internal_err(
                "attempting to read inner on readonly consul authority"
            ))
        }
    }

    fn write_inner(&self) -> Result<RwLockWriteGuard<'_, ConsulAuthorityInner>, Error> {
        if let Some(inner_mutex) = &self.inner {
            match inner_mutex.write() {
                Ok(inner) => Ok(inner),
                Err(e) => bail!(internal_err(format!("rwlock is poisoned: '{}'", e))),
            }
        } else {
            bail!(internal_err(
                "attempting to mutate inner on readonly consul authority"
            ))
        }
    }

    fn update_controller_index_from_pair(&self, kv: &KVPair) -> Result<(), Error> {
        let mut inner = self.write_inner()?;

        let new_index = match (kv.ModifyIndex, kv.CreateIndex) {
            (Some(modify), _) => modify,
            (_, Some(create)) => create,
            _ => 0,
        };

        inner.controller_index = Some(new_index);
        Ok(())
    }

    fn prefix_with_deployment(&self, path: &str) -> String {
        self.deployment.clone() + "/" + path
    }
}

fn is_new_index(current_index: Option<u64>, kv: &KVPair) -> bool {
    if let Some(current) = current_index {
        match (kv.ModifyIndex, kv.CreateIndex) {
            (Some(modify), _) => modify > current,
            (_, Some(create)) => create > current,
            _ => true,
        }
    } else {
        true
    }
}

impl Drop for ConsulAuthority {
    fn drop(&mut self) {
        let inner = self.read_inner().unwrap();
        if let Some(session) = &inner.session {
            // This will not be populated without an id.
            #[allow(clippy::unwrap_used)]
            let _ = futures::executor::block_on(
                self.consul.destroy(session.ID.as_ref().unwrap(), None),
            );
        }
    }
}

#[async_trait]
impl AuthorityControl for ConsulAuthority {
    async fn init(&self) -> Result<(), Error> {
        self.create_session().await
    }

    async fn become_leader(&self, payload: LeaderPayload) -> Result<Option<LeaderPayload>, Error> {
        // Move session creation to the start of the authority.
        let session = self.get_session()?;

        let key = self.prefix_with_deployment(CONTROLLER_KEY);
        let pair = KVPair {
            Key: key.clone(),
            Value: serde_json::to_string(&payload)?,
            Session: Some(session),
            ..Default::default()
        };

        // Acquire will only write a new Value for the KVPair if no other leader
        // holds the lock. The lock is released if a leader's session dies.
        match self.consul.acquire(&pair, None).await {
            Ok((true, _)) => {
                // Perform a get to update this authorities internal index.
                if let Ok((Some(kv), _)) = self.consul.get(&key, None).await {
                    if kv.Session == pair.Session {
                        self.update_controller_index_from_pair(&kv)?;
                    }
                }

                Ok(Some(payload))
            }
            Ok((false, _)) => Ok(None),
            Err(_) => Err(anyhow!("Failed")),
        }
    }

    async fn surrender_leadership(&self) -> Result<(), Error> {
        let session = self.get_session()?;

        let pair = KVPair {
            Key: self.prefix_with_deployment(CONTROLLER_KEY),
            Session: Some(session),
            ..Default::default()
        };

        // If we currently hold the lock on CONTROLLER_KEY, we will relinquish it.
        match self.consul.release(&pair, None).await {
            Ok(_) => Ok(()),
            Err(e) => bail!(e.to_string()),
        }
    }

    // Block until there is any leader.
    async fn get_leader(&self) -> Result<LeaderPayload, Error> {
        loop {
            if let Some(leader) = self.try_read::<LeaderPayload>(CONTROLLER_KEY).await? {
                return Ok(leader);
            }
            // TODO(justin): Thread parking potentially.
        }
    }

    async fn try_get_leader(&self) -> Result<GetLeaderResult, Error> {
        // Scope `inner` to this block as it is not Send and cannot be held
        // when we hit an await.
        let current_index = {
            let inner = self.read_inner()?;
            inner.controller_index
        };

        Ok(
            match self
                .consul
                .get(&self.prefix_with_deployment(CONTROLLER_KEY), None)
                .await
            {
                Ok((Some(kv), _)) if is_new_index(current_index, &kv) => {
                    self.update_controller_index_from_pair(&kv)?;
                    // Consul encodes all responses as base64. Using ?raw to get the
                    // raw value back breaks the client we are using.
                    let bytes = base64::decode(kv.Value)?;
                    let as_str = serde_json::from_slice::<String>(&bytes)?;
                    GetLeaderResult::NewLeader(serde_json::from_str(&as_str)?)
                }
                Ok((Some(_), _)) => GetLeaderResult::Unchanged,
                _ => GetLeaderResult::NoLeader,
            },
        )
    }

    fn can_watch(&self) -> bool {
        false
    }

    async fn watch_leader(&self) -> Result<(), Error> {
        Ok(())
    }

    async fn watch_workers(&self) -> Result<(), Error> {
        Ok(())
    }

    async fn try_read<P: DeserializeOwned>(&self, path: &str) -> Result<Option<P>, Error> {
        Ok(
            match self
                .consul
                .get(&self.prefix_with_deployment(path), None)
                .await
            {
                Ok((Some(kv), _)) => {
                    // Consul encodes all responses as base64. Using ?raw to get the
                    // raw value back breaks the client we are using.
                    let bytes = base64::decode(kv.Value)?;
                    let as_str = serde_json::from_slice::<String>(&bytes)?;
                    Some(serde_json::from_str(&as_str)?)
                }
                Ok((None, _)) => None,
                // The API currently throws an error that it cannot parse the json
                // if the key does not exist.
                Err(_) => None,
            },
        )
    }

    async fn read_modify_write<F, P, E>(&self, path: &str, mut f: F) -> Result<Result<P, E>, Error>
    where
        F: Send + FnMut(Option<P>) -> Result<P, E>,
        P: Send + Serialize + DeserializeOwned,
        E: Send,
    {
        loop {
            // TODO(justin): Use cas parameter to only modify if we have the same
            // ModifyIndex when we write.
            let current_val = self.try_read(path).await?;
            let modified = f(current_val);

            if let Ok(r) = modified {
                let new_val = serde_json::to_string(&r)?;
                // TODO(justin): Write wrapper.
                let pair = KVPair {
                    Key: self.prefix_with_deployment(path),
                    Value: new_val,
                    ..Default::default()
                };

                match self.consul.put(&pair, None).await {
                    Ok((true, _)) => return Ok(Ok(r)),
                    Ok((false, _)) => continue,
                    Err(e) => bail!(e.to_string()),
                }
            }
        }
    }

    async fn update_controller_state<F, P, E>(&self, f: F) -> Result<Result<P, E>, Error>
    where
        F: Send + FnMut(Option<P>) -> Result<P, E>,
        P: Send + Serialize + DeserializeOwned,
        E: Send,
    {
        self.read_modify_write(&self.prefix_with_deployment(STATE_KEY), f)
            .await
    }

    async fn try_read_raw(&self, path: &str) -> Result<Option<Vec<u8>>, Error> {
        Ok(
            match self
                .consul
                .get(&self.prefix_with_deployment(path), None)
                .await
            {
                Ok((Some(kv), _)) => {
                    // Consul encodes all responses as base64.
                    let bytes = base64::decode(kv.Value)?;
                    let as_str = serde_json::from_slice::<String>(&bytes)?;
                    Some(as_str.as_bytes().to_vec())
                }
                // The API currently throws an error that it cannot parse the json
                // if the key does not exist.
                Ok((None, _)) => None,
                Err(e) => bail!(e.to_string()),
            },
        )
    }

    async fn register_worker(&self, payload: WorkerDescriptor) -> Result<Option<WorkerId>, Error>
    where
        WorkerDescriptor: Serialize,
    {
        // Each worker is associated with the key:
        // `WORKER_PREFIX`/<session>.
        let session = self.get_session()?;
        let key = worker_id_to_path(&session);

        let pair = KVPair {
            Key: self.prefix_with_deployment(&key),
            Value: serde_json::to_string(&payload)?,
            Session: Some(session.clone()),
            ..Default::default()
        };

        // Acquire will only write a new Value for the KVPair if no other leader
        // holds the lock. The lock is released if a leader's session dies.
        match self.consul.acquire(&pair, None).await {
            Ok(_) => Ok(Some(session)),
            Err(e) => bail!(e.to_string()),
        }
    }

    async fn worker_heartbeat(
        &self,
        id: WorkerId,
    ) -> Result<AuthorityWorkerHeartbeatResponse, Error> {
        //TODO(justin): Consider changing this to heartbeat without a parameter.
        Ok(match self.consul.renew(&id, None).await {
            Ok(_) => AuthorityWorkerHeartbeatResponse::Alive,
            Err(_) => AuthorityWorkerHeartbeatResponse::Failed,
        })
    }

    async fn get_workers(&self) -> Result<HashSet<WorkerId>, Error> {
        Ok(
            match consul::kv::KV::list(
                &self.consul,
                &self.prefix_with_deployment(WORKER_PREFIX),
                None,
            )
            .await
            {
                Ok((children, _)) => children
                    .into_iter()
                    .map(|kv| path_to_worker_id(&kv.Key))
                    .collect(),
                // The API currently throws an error that it cannot parse the json
                // if the key does not exist.
                Err(e) => bail!(e.to_string()),
            },
        )
    }

    async fn worker_data(
        &self,
        worker_ids: Vec<WorkerId>,
    ) -> Result<HashMap<WorkerId, WorkerDescriptor>, Error> {
        let mut worker_descriptors: HashMap<WorkerId, WorkerDescriptor> = HashMap::new();

        for w in worker_ids {
            if let Ok((Some(kv), _)) = self
                .consul
                .get(&self.prefix_with_deployment(&worker_id_to_path(&w)), None)
                .await
            {
                // Consul encodes all responses as base64. Using ?raw to get the
                // raw value back breaks the client we are using.
                let bytes = base64::decode(kv.Value)?;
                let as_str = serde_json::from_slice::<String>(&bytes)?;
                worker_descriptors.insert(w, serde_json::from_str(&as_str)?);
            }
        }

        Ok(worker_descriptors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::Url;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test]
    #[ignore]
    async fn read_write_operations() {
        let authority =
            Arc::new(ConsulAuthority::new("http://127.0.0.1:8500/read_write_operations").unwrap());

        assert!(authority.try_read::<Duration>("a").await.unwrap().is_none());
        assert_eq!(
            authority
                .read_modify_write("a", |_: Option<Duration>| -> Result<Duration, Duration> {
                    Ok(Duration::from_secs(10))
                })
                .await
                .unwrap(),
            Ok(Duration::from_secs(10))
        );
        assert_eq!(
            authority.try_read::<Duration>("a").await.unwrap(),
            Some(Duration::from_secs(10))
        );
    }

    #[tokio::test]
    #[ignore]
    async fn leader_election_operations() {
        let authority_address = "http://127.0.0.1:8500/leader_election";
        let authority = Arc::new(ConsulAuthority::new(authority_address).unwrap());

        let payload = LeaderPayload {
            controller_uri: url::Url::parse("http://127.0.0.1:2181").unwrap(),
            nonce: 1,
        };
        let expected_leader_payload = payload.clone();
        assert_eq!(
            authority.become_leader(payload.clone()).await.unwrap(),
            Some(payload)
        );
        assert_eq!(
            &authority.get_leader().await.unwrap(),
            &expected_leader_payload
        );

        // This authority can't become the leader because it doesn't hold the lock on the
        // leader key.
        let authority_2 = Arc::new(ConsulAuthority::new(authority_address).unwrap());
        let payload_2 = LeaderPayload {
            controller_uri: url::Url::parse("http://127.0.0.1:2182").unwrap(),
            nonce: 2,
        };
        // Attempt to become leader, but fail as the other leader still lives.
        authority_2.become_leader(payload_2.clone()).await.unwrap();
        assert_eq!(
            &authority.get_leader().await.unwrap(),
            &expected_leader_payload
        );

        // Regicide.
        drop(authority);

        // Since the previous leader has died, we should be able to now become the leader.
        authority_2.become_leader(payload_2.clone()).await.unwrap();
        assert_eq!(&authority_2.get_leader().await.unwrap(), &payload_2);

        // Surrender leadership willingly but keep the session alive.
        authority_2.surrender_leadership().await.unwrap();

        let authority_3 = Arc::new(ConsulAuthority::new(authority_address).unwrap());
        let payload_3 = LeaderPayload {
            controller_uri: url::Url::parse("http://127.0.0.1:2183").unwrap(),
            nonce: 3,
        };
        authority_3.become_leader(payload_3.clone()).await.unwrap();
        assert_eq!(&authority_3.get_leader().await.unwrap(), &payload_3);
    }

    #[tokio::test]
    #[ignore]
    async fn retrieve_workers() {
        let authority_address = "http://127.0.0.1:8500/retrieve_workers";
        let authority = Arc::new(ConsulAuthority::new(authority_address).unwrap());

        let worker = WorkerDescriptor {
            worker_uri: Url::parse("http://127.0.0.1").unwrap(),
            reader_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 1234),
            region: None,
            reader_only: false,
            volume_id: None,
        };

        let workers = authority.get_workers().await.unwrap();
        assert!(workers.is_empty());

        let worker_id = authority
            .register_worker(worker.clone())
            .await
            .unwrap()
            .unwrap();
        let workers = authority.get_workers().await.unwrap();

        assert_eq!(workers.len(), 1);
        assert!(workers.contains(&worker_id));
        assert_eq!(
            authority.worker_heartbeat(worker_id.clone()).await.unwrap(),
            AuthorityWorkerHeartbeatResponse::Alive
        );
        assert_eq!(
            worker,
            authority
                .worker_data(vec![worker_id.clone()])
                .await
                .unwrap()[&worker_id]
        );

        let authority_2 = Arc::new(ConsulAuthority::new(authority_address).unwrap());
        let worker_id = authority_2.register_worker(worker).await.unwrap().unwrap();
        let workers = authority_2.get_workers().await.unwrap();
        assert_eq!(workers.len(), 2);
        assert!(workers.contains(&worker_id));

        // Kill the session, this should remove the keys from the worker set.
        drop(authority);
        drop(authority_2);

        let authority = Arc::new(ConsulAuthority::new(authority_address).unwrap());
        let workers = authority.get_workers().await.unwrap();
        assert_eq!(workers.len(), 0);
    }

    #[tokio::test]
    #[ignore]
    async fn leader_indexes() {
        let authority_address = "http://127.0.0.1:8500/leader_indexes";
        let authority = Arc::new(ConsulAuthority::new(authority_address).unwrap());

        let payload = LeaderPayload {
            controller_uri: url::Url::parse("http://127.0.0.1:2181").unwrap(),
            nonce: 1,
        };

        assert_eq!(
            authority.try_get_leader().await.unwrap(),
            GetLeaderResult::NoLeader,
        );

        assert_eq!(
            authority.become_leader(payload.clone()).await.unwrap(),
            Some(payload)
        );
        // This should be unchanged as the index should have been set in
        // `become_leader`.
        assert_eq!(
            authority.try_get_leader().await.unwrap(),
            GetLeaderResult::Unchanged,
        );
        drop(authority);

        let authority = Arc::new(ConsulAuthority::new(authority_address).unwrap());
        assert_eq!(
            authority.try_get_leader().await.unwrap(),
            GetLeaderResult::NoLeader,
        );
    }
}
