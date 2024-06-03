use {
    super::etcd_path::{get_instance_lock_prefix_v1, get_leader_state_log_key_v1},
    crate::scylladb::{
        etcd_utils::{
            self,
            barrier::{get_barrier, Barrier},
            lease::ManagedLease,
        },
        types::{ConsumerGroupId, ProducerId},
        yellowstone_log::consumer_group::etcd_path::get_producer_lock_path_v1,
    },
    bincode::{deserialize, serialize},
    etcd_client::{Compare, GetOptions, PutOptions, TxnOp, WatchOptions},
    futures::{future, Future, FutureExt},
    serde::{Deserialize, Serialize},
    std::{fmt, time::Duration},
    thiserror::Error,
    tokio::sync::oneshot::{self, error::RecvError},
    tracing::warn,
    uuid::Uuid,
};

// enum ConsumerGroupLeaderLocation(
//     Local,
//     Remote()
// )

#[derive(Serialize, Deserialize)]
enum LeaderCommand {
    Join { lock_key: Vec<u8> },
}

// enum ConsumerGroupLeaderState {
//     Idle,
//     HandlingJoinRequest {
//         lock_key: Vec<u8>,
//         instance_id: InstanceId,
//     },
//     HandlingTimelineTranslation {
//         from_producer_id: ProducerId,
//         target_producer_id: Produ
//     },
//     Poisoned,
// }

///
/// Cancel safe producer dead signal
struct ProducerDeadSignal {
    // When this object is drop, the sender will drop too and cancel the watch automatically
    _cancel_watcher_tx: oneshot::Sender<()>,
    inner: oneshot::Receiver<()>,
}

impl Future for ProducerDeadSignal {
    type Output = Result<(), RecvError>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let inner = &mut self.inner;
        tokio::pin!(inner);
        inner.poll(cx)
    }
}

async fn get_producer_dead_signal(
    mut etcd: etcd_client::Client,
    producer_id: ProducerId,
) -> anyhow::Result<ProducerDeadSignal> {
    let producer_lock_path = get_producer_lock_path_v1(producer_id);
    let (mut watch_handle, mut stream) = etcd
        .watch(
            producer_lock_path.as_bytes(),
            Some(WatchOptions::new().with_prefix()),
        )
        .await?;

    let (tx, rx) = oneshot::channel();
    let get_resp = etcd.get(producer_lock_path.as_bytes(), None).await?;

    let (cancel_watch_tx, cancel_watch_rx) = oneshot::channel::<()>();

    tokio::spawn(async move {
        let _ = cancel_watch_rx.await;
        let _ = watch_handle.cancel().await;
    });

    // If the producer is already dead, we can quit early
    if get_resp.count() == 0 {
        tx.send(())
            .map_err(|_| anyhow::anyhow!("failed to early notify dead producer"))?;
        return Ok(ProducerDeadSignal {
            _cancel_watcher_tx: cancel_watch_tx,
            inner: rx,
        });
    }

    tokio::spawn(async move {
        while let Some(message) = stream
            .message()
            .await
            .expect("watch stream was terminated early")
        {
            let ev_type = message
                .events()
                .first()
                .map(|ev| ev.event_type())
                .expect("watch received a none event");
            match ev_type {
                etcd_client::EventType::Put => {
                    panic!("corrupted system state, producer was created after dead signal")
                }
                etcd_client::EventType::Delete => {
                    if tx.send(()).is_err() {
                        warn!("producer dead signal receiver half was terminated before signal was send");
                    }
                    break;
                }
            }
        }
    });
    Ok(ProducerDeadSignal {
        _cancel_watcher_tx: cancel_watch_tx,
        inner: rx,
    })
}

type EtcdKey = Vec<u8>;

#[derive(Serialize, Deserialize, Clone, Debug, Eq, PartialEq)]
enum ConsumerGroupLeaderSM {
    Init,
    LostProducer {
        lost_producer_id: ProducerId,
        execution_id: Vec<u8>,
    },
    WaitingBarrier {
        lease_id: i64,
        barrier_key: EtcdKey,
        wait_for: Vec<EtcdKey>,
    },
    ComputingProducerSelection,
    Idle {
        producer_id: ProducerId,
        execution_id: Vec<u8>,
    },
}

#[derive(Serialize, Deserialize)]
struct ConsumerGroupLeaderState {
    consumer_group_id: ConsumerGroupId,
    state_machine: ConsumerGroupLeaderSM,
}

pub struct ConsumerGroupLeaderNode {
    consumer_group_id: ConsumerGroupId,
    etcd: etcd_client::Client,
    leader_key: EtcdKey,
    leader_lease: ManagedLease,
    state_machine: ConsumerGroupLeaderSM,
    last_revision: i64,
    producer_dead_signal: Option<ProducerDeadSignal>,
    barrier: Option<Barrier>,
}

///
/// This error is raised when there is no active producer for the desired commitment level.
///
#[derive(Copy, Error, PartialEq, Eq, Debug, Clone)]
pub enum LeaderInitError {
    FailedToUpdateStateLog,
}

impl fmt::Display for LeaderInitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LeaderInitError::FailedToUpdateStateLog => f.write_str("FailedToUpdateStateLog"),
        }
    }
}

impl ConsumerGroupLeaderNode {
    pub async fn new(
        mut etcd: etcd_client::Client,
        leader_key: EtcdKey,
        leader_lease: ManagedLease,
        consumer_group_id: ConsumerGroupId,
    ) -> anyhow::Result<Self> {
        let leader_log_key = get_leader_state_log_key_v1(consumer_group_id.clone());
        let get_resp = etcd.get(leader_log_key.as_str(), None).await?;
        let maybe_last_leader_state = get_resp
            .kvs()
            .first()
            .map(|kv| {
                deserialize::<ConsumerGroupLeaderSM>(kv.value()).map(|sm| (kv.mod_revision(), sm))
            })
            .transpose()?;

        let (last_revision, state_machine) = match maybe_last_leader_state {
            Some((revision, last_leader_state)) => (revision, last_leader_state),
            None => {
                let init_state = ConsumerGroupLeaderSM::Init;
                let txn = etcd_client::Txn::new()
                    .when(vec![
                        Compare::version(leader_key.as_slice(), etcd_client::CompareOp::Greater, 0),
                        Compare::version(leader_log_key.as_str(), etcd_client::CompareOp::Equal, 0),
                    ])
                    .and_then(vec![TxnOp::put(
                        leader_log_key.as_str(),
                        serialize(&init_state)?,
                        None,
                    )]);
                let txn_resp = etcd.txn(txn).await?;
                let revision = txn_resp
                    .op_responses()
                    .pop()
                    .and_then(|op| match op {
                        etcd_client::TxnOpResponse::Put(put_resp) => {
                            put_resp.header().map(|header| header.revision())
                        }
                        _ => panic!("unexpected op"),
                    })
                    .ok_or(LeaderInitError::FailedToUpdateStateLog)?;
                (revision, init_state)
            }
        };

        //let producer_dead_signal = get_producer_dead_signal(etcd.clone(), producer_id).await?;
        let ret = ConsumerGroupLeaderNode {
            consumer_group_id,
            etcd,
            leader_key,
            leader_lease,
            producer_dead_signal: None,
            state_machine,
            last_revision,
            barrier: None,
        };
        Ok(ret)
    }

    pub async fn leader_loop(
        &mut self,
        mut interrupt_signal: oneshot::Receiver<()>,
    ) -> anyhow::Result<()> {
        let leader_log_key = get_leader_state_log_key_v1(self.consumer_group_id.clone());
        loop {
            let next_state = match &self.state_machine {
                ConsumerGroupLeaderSM::Init => ConsumerGroupLeaderSM::ComputingProducerSelection,
                ConsumerGroupLeaderSM::LostProducer {
                    lost_producer_id,
                    execution_id,
                } => {
                    let barrier_key = Uuid::new_v4();
                    let lease_id = self.etcd.lease_grant(10, None).await?.id();
                    let lock_prefix = get_instance_lock_prefix_v1(self.consumer_group_id.clone());
                    // TODO add healthcheck here
                    let wait_for = self
                        .etcd
                        .get(lock_prefix, Some(GetOptions::new().with_prefix()))
                        .await?
                        .kvs()
                        .iter()
                        .map(|kv| kv.key().to_vec())
                        .collect::<Vec<_>>();

                    let barrier = etcd_utils::barrier::new_barrier(
                        self.etcd.clone(),
                        barrier_key.as_bytes(),
                        &wait_for,
                        lease_id,
                    )
                    .await?;
                    self.barrier = Some(barrier);

                    let next_state = ConsumerGroupLeaderSM::WaitingBarrier {
                        lease_id,
                        barrier_key: barrier_key.as_bytes().to_vec(),
                        wait_for,
                    };
                    next_state
                }
                ConsumerGroupLeaderSM::WaitingBarrier {
                    barrier_key,
                    wait_for,
                    lease_id,
                } => {
                    let barrier = if let Some(barrier) = self.barrier.take() {
                        barrier
                    } else {
                        get_barrier(self.etcd.clone(), &barrier_key).await?
                    };

                    tokio::select! {
                        _ = &mut interrupt_signal => return Ok(()),
                        _ = barrier.wait() => ()
                    }
                    ConsumerGroupLeaderSM::ComputingProducerSelection
                }
                ConsumerGroupLeaderSM::ComputingProducerSelection => {
                    todo!()
                }
                ConsumerGroupLeaderSM::Idle {
                    producer_id,
                    execution_id,
                } => {
                    let signal = self.producer_dead_signal.get_or_insert(
                        get_producer_dead_signal(self.etcd.clone(), *producer_id).await?,
                    );
                    tokio::select! {
                        _ = &mut interrupt_signal => return Ok(()),
                        _ = signal => {
                            warn!("received dead signal from producer {producer_id:?}");
                            let barrier_key = Uuid::new_v4();
                            let lease_id = self.etcd.lease_grant(10, None).await?.id();
                            self.etcd.put(barrier_key.as_bytes(), [], Some(PutOptions::new().with_lease(lease_id))).await?;

                            ConsumerGroupLeaderSM::LostProducer {
                                lost_producer_id: *producer_id,
                                execution_id: execution_id.clone()
                            }
                        }
                    }
                }
            };

            let txn = etcd_client::Txn::new()
                .when(vec![
                    Compare::version(
                        self.leader_key.as_slice(),
                        etcd_client::CompareOp::Greater,
                        0,
                    ),
                    Compare::mod_revision(
                        leader_log_key.as_str(),
                        etcd_client::CompareOp::Less,
                        self.last_revision,
                    ),
                ])
                .and_then(vec![TxnOp::put(
                    leader_log_key.as_str(),
                    serialize(&next_state)?,
                    None,
                )]);
            let txn_resp = self.etcd.txn(txn).await?;
            let revision = txn_resp
                .op_responses()
                .pop()
                .and_then(|op| match op {
                    etcd_client::TxnOpResponse::Put(put_resp) => {
                        put_resp.header().map(|header| header.revision())
                    }
                    _ => panic!("unexpected op"),
                })
                .ok_or(LeaderInitError::FailedToUpdateStateLog)?;

            self.last_revision = revision;
            self.state_machine = next_state;

            match interrupt_signal.try_recv() {
                Ok(_) => return Ok(()),
                Err(oneshot::error::TryRecvError::Empty) => continue,
                Err(oneshot::error::TryRecvError::Closed) => return Ok(()),
            }
        }
    }
}

// pub struct LeaderStateLog {
//     watcher,
// }
