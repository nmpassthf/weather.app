mod failure;
mod pending;
mod session;

use std::{
    collections::VecDeque,
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use prost::Message;
use prost::bytes::Bytes;
use tokio::{
    sync::{broadcast, mpsc},
    time::{Instant, timeout_at},
};
use weather_schema::*;
use zeromq::{
    DealerRecvHalf, DealerSendHalf, Socket, SocketRecv, SocketSend, SubSocket, ZmqMessage,
};

pub(crate) use self::failure::RemoteRpcError;
use self::{
    failure::ClientFailure,
    pending::{PendingLease, PendingRegistry, RegisterError},
    session::{ClientSession, SessionTaskResult},
};
use crate::pagination::PageCursor;

const ENGINE_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const RPC_SEND_QUEUE: usize = 64;

/// 引擎客户端：DEALER 走 RPC，SUB 订阅 PUB 事件。
#[derive(Clone)]
pub(crate) struct EngineClient {
    inner: Arc<ClientInner>,
}

struct ClientInner {
    session: ClientSession,
    rpc_send: mpsc::Sender<Vec<u8>>,
    pending: PendingRegistry,
    events: Mutex<broadcast::Receiver<EngineEvent>>,
    hmac_key: Option<[u8; 32]>,
}

impl Drop for ClientInner {
    fn drop(&mut self) {
        self.session.request_close();
    }
}

/// 解析后的事件，附带 topic。
#[derive(Debug, Clone)]
pub(crate) struct EngineEvent {
    pub topic: String,
    pub envelope: EventEnvelope,
}

pub(crate) fn require_config(
    config: Option<weather_schema::AppConfig>,
    operation: &str,
) -> Result<weather_schema::AppConfig> {
    config.with_context(|| format!("engine {operation} response is missing config"))
}

impl EngineClient {
    /// 连接到 RPC 与 PUB endpoint，启动收发后台任务。
    pub(crate) async fn connect(
        rpc_endpoint: String,
        pub_endpoint: String,
        hmac_key: Option<[u8; 32]>,
    ) -> Result<Self> {
        let mut dealer = zeromq::DealerSocket::new();
        dealer
            .connect(&rpc_endpoint)
            .await
            .with_context(|| format!("failed to connect RPC endpoint {rpc_endpoint}"))?;

        // Finish all fallible socket setup before spawning any background task.
        let mut subscriber = SubSocket::new();
        subscriber
            .connect(&pub_endpoint)
            .await
            .with_context(|| format!("failed to connect PUB endpoint {pub_endpoint}"))?;
        subscriber
            .subscribe("")
            .await
            .context("failed to subscribe PUB topics")?;

        let (rpc_send_half, rpc_receive_half) = dealer.split();
        let (rpc_send, rpc_outbox) = mpsc::channel::<Vec<u8>>(RPC_SEND_QUEUE);
        let pending = PendingRegistry::new();
        let (events_tx, events) = broadcast::channel(256);
        let session = ClientSession::spawn(
            pending.clone(),
            run_rpc_sender(rpc_send_half, rpc_outbox),
            run_rpc_receiver(rpc_receive_half, pending.clone()),
            run_event_receiver(subscriber, events_tx),
        );

        Ok(Self {
            inner: Arc::new(ClientInner {
                session,
                rpc_send,
                pending,
                events: Mutex::new(events),
                hmac_key,
            }),
        })
    }

    /// 订阅引擎事件流。
    pub(crate) fn subscribe_events(&self) -> broadcast::Receiver<EngineEvent> {
        self.inner
            .events
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .resubscribe()
    }

    /// 关闭共享客户端会话并等待所有 socket task 退出。
    pub(crate) async fn close(&self) {
        self.inner.session.close().await;
    }

    pub(crate) async fn status(&self) -> Result<EngineStatus> {
        self.request(RpcKind::GetEngineStatus, Empty {}).await
    }

    pub(crate) async fn shutdown(&self) -> Result<Empty> {
        self.request(RpcKind::Shutdown, Empty {}).await
    }

    pub(crate) async fn shutdown_if_owned(&self, owner_token: &str) -> Result<Empty> {
        self.request(
            RpcKind::Shutdown,
            ShutdownRequest {
                owner_token: Some(owner_token.to_string()),
            },
        )
        .await
    }

    pub(crate) async fn configured_stations(
        &self,
        offset: u32,
        page_size: u32,
    ) -> Result<ListConfiguredStationsResponse> {
        self.request(
            RpcKind::ListConfiguredStations,
            ListConfiguredStationsRequest {
                page_offset: offset,
                page_size,
            },
        )
        .await
    }

    pub(crate) async fn all_configured_stations(&self) -> Result<ListConfiguredStationsResponse> {
        collect_configured_stations(|offset, page_size| self.configured_stations(offset, page_size))
            .await
    }

    /// 拉取 engine 当前 config（`defaults=true` 拿默认模板）。
    pub(crate) async fn get_config(&self, defaults: bool) -> Result<GetConfigResponse> {
        self.request(RpcKind::GetConfig, GetConfigRequest { defaults })
            .await
    }

    /// 下发整份 config，engine validate + 锁定不可变字段 + 持久化后返回最终值。
    pub(crate) async fn update_config(
        &self,
        config: weather_schema::AppConfig,
    ) -> Result<UpdateConfigResponse> {
        self.request(
            RpcKind::UpdateConfig,
            UpdateConfigRequest {
                config: Some(config),
            },
        )
        .await
    }

    /// 发送一个 RPC 请求并等待响应。
    pub(crate) async fn request<Req, Resp>(&self, kind: RpcKind, payload: Req) -> Result<Resp>
    where
        Req: Message,
        Resp: Message + Default,
    {
        let timeout = ENGINE_REQUEST_TIMEOUT;
        let deadline = Instant::now() + timeout;
        let payload = payload.encode_to_vec();
        let (request_id, lease) = loop {
            if Instant::now() >= deadline {
                return Err(request_timeout_error(timeout));
            }
            let request_id = correlation_id("tui-request");
            match self.inner.pending.register(request_id.clone()) {
                Ok(lease) => break (request_id, lease),
                Err(RegisterError::Collision) => continue,
                Err(RegisterError::Terminal(failure)) => {
                    return Err(anyhow::Error::new(failure));
                }
            }
        };
        let mut envelope = RpcRequest {
            schema_version: SCHEMA_VERSION.to_string(),
            request_id,
            kind: kind as i32,
            timestamp_unix_ms: unix_timestamp_ms().unwrap_or_default(),
            hmac_sha256: Vec::new(),
            payload,
        };
        if let Some(key) = self.inner.hmac_key {
            envelope.hmac_sha256 = weather_schema::rpc_request_hmac(&envelope, &key)?;
        }
        let response = dispatch_request(
            self.inner.rpc_send.clone(),
            envelope.encode_to_vec(),
            lease,
            deadline,
            timeout,
        )
        .await?;
        if response.status == ResponseStatus::Error as i32 {
            let error = response.error.map_or_else(
                RemoteRpcError::missing_engine_error,
                RemoteRpcError::from_engine_error,
            );
            return Err(anyhow::Error::new(error));
        }
        Ok(Resp::decode(response.payload.as_slice())?)
    }
}

async fn collect_configured_stations<Fetch, FetchFuture>(
    mut fetch: Fetch,
) -> Result<ListConfiguredStationsResponse>
where
    Fetch: FnMut(u32, u32) -> FetchFuture,
    FetchFuture: Future<Output = Result<ListConfiguredStationsResponse>>,
{
    let mut cursor = PageCursor::default();
    let mut combined = ListConfiguredStationsResponse::default();
    loop {
        let (offset, page_size) = cursor.request(MAX_RPC_PAGE_SIZE)?;
        let mut page = fetch(offset, page_size).await?;
        let has_more = page.has_more;
        let next_offset = page.next_offset;
        combined.stations.append(&mut page.stations);
        combined.has_more = has_more;
        combined.next_offset = next_offset;
        if !cursor.advance(has_more, next_offset)? {
            return Ok(combined);
        }
    }
}

async fn dispatch_request(
    rpc_send: mpsc::Sender<Vec<u8>>,
    encoded_request: Vec<u8>,
    mut lease: PendingLease,
    deadline: Instant,
    timeout: Duration,
) -> Result<RpcResponse> {
    let exchange = async move {
        rpc_send
            .send(encoded_request)
            .await
            .map_err(|_| ClientFailure::rpc_send("request queue closed"))?;
        lease.receive().await
    };
    match timeout_at(deadline, exchange).await {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(failure)) => Err(anyhow::Error::new(failure)),
        Err(_) => Err(request_timeout_error(timeout)),
    }
}

fn request_timeout_error(timeout: Duration) -> anyhow::Error {
    anyhow!("RPC request timed out after {timeout:?}")
}

async fn run_rpc_sender(
    mut socket: DealerSendHalf,
    mut outbox: mpsc::Receiver<Vec<u8>>,
) -> SessionTaskResult {
    while let Some(payload) = outbox.recv().await {
        let mut frames = VecDeque::new();
        frames.push_back(Bytes::from(payload));
        let message = ZmqMessage::try_from(frames).map_err(ClientFailure::rpc_send)?;
        socket
            .send(message)
            .await
            .map_err(ClientFailure::rpc_send)?;
    }
    Ok(())
}

async fn run_rpc_receiver(
    mut socket: DealerRecvHalf,
    pending: PendingRegistry,
) -> SessionTaskResult {
    loop {
        let message = socket.recv().await.map_err(ClientFailure::rpc_receive)?;
        dispatch_rpc_response(message, &pending)?;
    }
}

fn dispatch_rpc_response(message: ZmqMessage, pending: &PendingRegistry) -> SessionTaskResult {
    let mut frames = message.into_vecdeque();
    let payload = frames
        .pop_front()
        .ok_or_else(|| ClientFailure::rpc_receive("received an empty RPC response message"))?;
    if !frames.is_empty() {
        return Err(ClientFailure::rpc_receive(
            "received unexpected extra RPC response frames",
        ));
    }
    let response = RpcResponse::decode(payload.as_ref())
        .map_err(|error| ClientFailure::rpc_receive(format_args!("invalid response: {error}")))?;
    // Unknown IDs are expected for responses that arrive after a request timed out.
    pending.complete(response);
    Ok(())
}

async fn run_event_receiver(
    mut subscriber: SubSocket,
    events: broadcast::Sender<EngineEvent>,
) -> SessionTaskResult {
    loop {
        let message = subscriber
            .recv()
            .await
            .map_err(ClientFailure::event_receive)?;
        let mut frames = message.into_vecdeque();
        let Some(topic_frame) = frames.pop_front() else {
            continue;
        };
        let Some(payload) = frames.pop_front() else {
            continue;
        };
        if !frames.is_empty() {
            continue;
        }
        let topic = String::from_utf8_lossy(topic_frame.as_ref()).to_string();
        let Ok(envelope) = EventEnvelope::decode(payload.as_ref()) else {
            continue;
        };
        let _ = events.send(EngineEvent { topic, envelope });
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, future::pending as never};

    use tokio::{sync::oneshot, time::advance};

    use super::*;

    #[test]
    fn missing_config_payload_is_rejected() {
        let error = require_config(None, "get-config").unwrap_err();

        assert_eq!(
            error.to_string(),
            "engine get-config response is missing config"
        );
    }

    fn configured_station(index: usize) -> ConfiguredStation {
        ConfiguredStation {
            name: format!("station-{index}"),
            enabled: true,
        }
    }

    #[tokio::test]
    async fn configured_station_collection_pages_all_257_entries() {
        let pages = RefCell::new(VecDeque::from([
            ListConfiguredStationsResponse {
                stations: (0..256).map(configured_station).collect(),
                has_more: true,
                next_offset: 256,
            },
            ListConfiguredStationsResponse {
                stations: vec![configured_station(256)],
                has_more: false,
                next_offset: 257,
            },
        ]));
        let requests = RefCell::new(Vec::new());

        let response = collect_configured_stations(|offset, page_size| {
            requests.borrow_mut().push((offset, page_size));
            std::future::ready(Ok(pages.borrow_mut().pop_front().unwrap()))
        })
        .await
        .unwrap();

        assert_eq!(response.stations.len(), 257);
        assert_eq!(response.stations[0].name, "station-0");
        assert_eq!(response.stations[256].name, "station-256");
        assert!(!response.has_more);
        assert_eq!(response.next_offset, 257);
        assert_eq!(
            requests.into_inner(),
            vec![(0, MAX_RPC_PAGE_SIZE), (256, MAX_RPC_PAGE_SIZE)]
        );
    }

    #[tokio::test]
    async fn configured_station_collection_honors_early_end() {
        let requests = RefCell::new(Vec::new());

        let response = collect_configured_stations(|offset, page_size| {
            requests.borrow_mut().push((offset, page_size));
            std::future::ready(Ok(ListConfiguredStationsResponse {
                stations: vec![configured_station(0)],
                has_more: false,
                next_offset: 1,
            }))
        })
        .await
        .unwrap();

        assert_eq!(response.stations.len(), 1);
        assert_eq!(requests.into_inner(), vec![(0, MAX_RPC_PAGE_SIZE)]);
    }

    struct DropNotice(Option<oneshot::Sender<()>>);

    impl Drop for DropNotice {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }

    async fn hold_forever<H: Send + 'static>(held: H, _notice: DropNotice) -> SessionTaskResult {
        let _held = held;
        never().await
    }

    fn test_client() -> (
        EngineClient,
        PendingRegistry,
        tokio::sync::watch::Receiver<bool>,
        Vec<oneshot::Receiver<()>>,
    ) {
        let pending = PendingRegistry::new();
        let (rpc_send, rpc_outbox) = mpsc::channel(1);
        let (events_tx, events) = broadcast::channel(1);
        let (send_dropped_tx, send_dropped_rx) = oneshot::channel();
        let (receive_dropped_tx, receive_dropped_rx) = oneshot::channel();
        let (event_dropped_tx, event_dropped_rx) = oneshot::channel();
        let session = ClientSession::spawn(
            pending.clone(),
            hold_forever(rpc_outbox, DropNotice(Some(send_dropped_tx))),
            hold_forever((), DropNotice(Some(receive_dropped_tx))),
            hold_forever(events_tx, DropNotice(Some(event_dropped_tx))),
        );
        let completion = session.completion();
        let client = EngineClient {
            inner: Arc::new(ClientInner {
                session,
                rpc_send,
                pending: pending.clone(),
                events: Mutex::new(events),
                hmac_key: None,
            }),
        };
        (
            client,
            pending,
            completion,
            vec![send_dropped_rx, receive_dropped_rx, event_dropped_rx],
        )
    }

    async fn wait_for_completion(completion: &mut tokio::sync::watch::Receiver<bool>) {
        loop {
            if *completion.borrow_and_update() {
                return;
            }
            completion.changed().await.unwrap();
        }
    }

    #[tokio::test]
    async fn request_cancellation_removes_pending_entry() {
        let pending = PendingRegistry::new();
        let lease = pending.register("request".to_string()).unwrap();
        let (rpc_send, mut outbox) = mpsc::channel(1);
        let task = tokio::spawn(dispatch_request(
            rpc_send,
            vec![1],
            lease,
            Instant::now() + ENGINE_REQUEST_TIMEOUT,
            ENGINE_REQUEST_TIMEOUT,
        ));

        assert_eq!(outbox.recv().await.unwrap(), vec![1]);
        assert_eq!(pending.len(), 1);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(pending.len(), 0);
    }

    #[tokio::test]
    async fn closed_request_queue_removes_pending_entry() {
        let pending = PendingRegistry::new();
        let lease = pending.register("request".to_string()).unwrap();
        let (rpc_send, outbox) = mpsc::channel(1);
        drop(outbox);

        let error = dispatch_request(
            rpc_send,
            vec![1],
            lease,
            Instant::now() + ENGINE_REQUEST_TIMEOUT,
            ENGINE_REQUEST_TIMEOUT,
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("request queue closed"));
        assert_eq!(pending.len(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn one_deadline_covers_queue_and_response_wait() {
        let pending = PendingRegistry::new();
        let lease = pending.register("request".to_string()).unwrap();
        let (rpc_send, mut outbox) = mpsc::channel(1);
        rpc_send.send(vec![0]).await.unwrap();
        let task = tokio::spawn(dispatch_request(
            rpc_send,
            vec![1],
            lease,
            Instant::now() + ENGINE_REQUEST_TIMEOUT,
            ENGINE_REQUEST_TIMEOUT,
        ));
        tokio::task::yield_now().await;

        advance(Duration::from_secs(20)).await;
        assert!(!task.is_finished());
        assert_eq!(outbox.recv().await.unwrap(), vec![0]);
        assert_eq!(outbox.recv().await.unwrap(), vec![1]);

        advance(Duration::from_secs(9)).await;
        tokio::task::yield_now().await;
        assert!(!task.is_finished());
        advance(Duration::from_secs(1)).await;
        let error = task.await.unwrap().unwrap_err();

        assert!(error.to_string().contains("timed out after 30s"));
        assert_eq!(pending.len(), 0);
    }

    #[tokio::test]
    async fn dropping_only_clone_stops_and_drains_the_session() {
        let (client, pending, mut completion, dropped) = test_client();
        let clone = client.clone();

        drop(client);
        tokio::task::yield_now().await;
        assert_eq!(pending.terminal_failure(), None);
        assert!(!*completion.borrow());

        drop(clone);
        wait_for_completion(&mut completion).await;
        assert_eq!(pending.terminal_failure(), Some(ClientFailure::Closed));
        for receiver in dropped {
            receiver.await.unwrap();
        }
    }

    #[tokio::test]
    async fn close_is_shared_idempotent_and_joins_the_session() {
        let (client, pending, mut completion, dropped) = test_client();
        let clone = client.clone();
        let mut lease = pending.register("in-flight".to_string()).unwrap();

        tokio::join!(client.close(), clone.close());

        assert_eq!(lease.receive().await.unwrap_err(), ClientFailure::Closed);
        wait_for_completion(&mut completion).await;
        for receiver in dropped {
            receiver.await.unwrap();
        }
        let error = clone.status().await.unwrap_err();
        assert_eq!(error.to_string(), "RPC client closed");
    }

    #[tokio::test]
    async fn event_subscriber_closes_when_the_session_stops() {
        let (client, _pending, _completion, dropped) = test_client();
        let mut events = client.subscribe_events();

        client.close().await;

        assert_eq!(
            events.recv().await.unwrap_err(),
            broadcast::error::RecvError::Closed
        );
        for receiver in dropped {
            receiver.await.unwrap();
        }
    }

    #[test]
    fn malformed_rpc_response_is_a_terminal_receive_failure() {
        let pending = PendingRegistry::new();
        let mut frames = VecDeque::new();
        frames.push_back(Bytes::from_static(&[0xff]));
        let message = ZmqMessage::try_from(frames).unwrap();

        let failure = dispatch_rpc_response(message, &pending).unwrap_err();

        assert!(matches!(failure, ClientFailure::RpcReceive(_)));
        assert!(failure.to_string().contains("invalid response"));
    }

    #[test]
    fn unknown_late_rpc_response_does_not_remove_an_active_waiter() {
        let pending = PendingRegistry::new();
        let active = pending.register("active".to_string()).unwrap();
        let response = RpcResponse {
            request_id: "already-timed-out".to_string(),
            ..Default::default()
        };
        let mut frames = VecDeque::new();
        frames.push_back(Bytes::from(response.encode_to_vec()));
        let message = ZmqMessage::try_from(frames).unwrap();

        dispatch_rpc_response(message, &pending).unwrap();

        assert_eq!(pending.len(), 1);
        drop(active);
        assert_eq!(pending.len(), 0);
    }
}
