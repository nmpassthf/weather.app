use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use prost::Message;
use prost::bytes::Bytes;
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};
use weather_schema::*;
use zeromq::{Socket, SocketRecv, SocketSend, SubSocket, ZmqMessage};

use crate::util::{now_ms, request_id};

const ENGINE_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const RPC_SEND_QUEUE: usize = 64;

/// 引擎客户端：DEALER 走 RPC，SUB 订阅 PUB 事件。
#[derive(Clone)]
pub(crate) struct EngineClient {
    rpc_send: mpsc::Sender<Vec<u8>>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<RpcResponse>>>>,
    events_tx: broadcast::Sender<EngineEvent>,
    hmac_key: Option<[u8; 32]>,
}

/// 解析后的事件，附带 topic。
#[derive(Debug, Clone)]
pub(crate) struct EngineEvent {
    pub topic: String,
    pub envelope: EventEnvelope,
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
        let (mut rpc_send_half, mut rpc_recv_half) = dealer.split();

        let (rpc_send, mut rpc_outbox) = mpsc::channel::<Vec<u8>>(RPC_SEND_QUEUE);
        tokio::spawn(async move {
            while let Some(payload) = rpc_outbox.recv().await {
                let mut frames = std::collections::VecDeque::new();
                frames.push_back(Bytes::from(payload));
                let Ok(message) = ZmqMessage::try_from(frames) else {
                    continue;
                };
                if rpc_send_half.send(message).await.is_err() {
                    break;
                }
            }
        });

        let pending: Arc<Mutex<HashMap<String, oneshot::Sender<RpcResponse>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let pending_clone = pending.clone();
        tokio::spawn(async move {
            while let Ok(message) = rpc_recv_half.recv().await {
                let frames = message.into_vecdeque();
                let Some(payload) = frames.into_iter().next() else {
                    continue;
                };
                let Ok(response) = RpcResponse::decode(payload.as_ref()) else {
                    continue;
                };
                let mut map = pending_clone.lock().await;
                if let Some(sender) = map.remove(&response.request_id) {
                    let _ = sender.send(response);
                }
            }
        });

        let (events_tx, _) = broadcast::channel(256);
        let events_tx_clone = events_tx.clone();
        let mut sub = SubSocket::new();
        sub.connect(&pub_endpoint)
            .await
            .with_context(|| format!("failed to connect PUB endpoint {pub_endpoint}"))?;
        sub.subscribe("")
            .await
            .context("failed to subscribe PUB topics")?;
        tokio::spawn(async move {
            while let Ok(message) = sub.recv().await {
                let mut frames = message.into_vecdeque();
                let Some(topic_frame) = frames.pop_front() else {
                    continue;
                };
                let Some(payload) = frames.pop_front() else {
                    continue;
                };
                let topic = String::from_utf8_lossy(topic_frame.as_ref()).to_string();
                let Ok(envelope) = EventEnvelope::decode(payload.as_ref()) else {
                    continue;
                };
                let _ = events_tx_clone.send(EngineEvent { topic, envelope });
            }
        });

        Ok(Self {
            rpc_send,
            pending,
            events_tx,
            hmac_key,
        })
    }

    /// 订阅引擎事件流。
    pub(crate) fn subscribe_events(&self) -> broadcast::Receiver<EngineEvent> {
        self.events_tx.subscribe()
    }

    pub(crate) async fn status(&self) -> Result<EngineStatus> {
        self.request(RpcKind::GetEngineStatus, Empty {}).await
    }

    pub(crate) async fn shutdown(&self) -> Result<Empty> {
        self.request(RpcKind::Shutdown, Empty {}).await
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

    /// 查询规范化站点名对应的 unified UUID，用于 GET_WEATHER 请求与 payload 过滤。
    pub(crate) async fn resolve_station_uuid(
        &self,
        name: &str,
    ) -> Result<ResolveStationUuidResponse> {
        self.request(
            RpcKind::ResolveStationUuid,
            ResolveStationUuidRequest {
                name: name.to_string(),
            },
        )
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
        let request_id = request_id();
        let mut envelope = RpcRequest {
            schema_version: SCHEMA_VERSION.to_string(),
            request_id: request_id.clone(),
            kind: kind as i32,
            timestamp_unix_ms: now_ms(),
            hmac_sha256: Vec::new(),
            payload: payload.encode_to_vec(),
        };
        if let Some(key) = self.hmac_key {
            envelope.hmac_sha256 = weather_schema::rpc_request_hmac(&envelope, &key)?;
        }
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(request_id.clone(), tx);
        if tokio::time::timeout(
            ENGINE_REQUEST_TIMEOUT,
            self.rpc_send.send(envelope.encode_to_vec()),
        )
        .await
        .context("timed out sending request to engine")?
        .is_err()
        {
            self.pending.lock().await.remove(&request_id);
            bail!("rpc send queue closed");
        }
        let response = tokio::time::timeout(ENGINE_REQUEST_TIMEOUT, rx)
            .await
            .context("timed out waiting for engine response")?;
        self.pending.lock().await.remove(&request_id);
        let response = response?;
        if response.status == ResponseStatus::Error as i32 {
            let err = response.error.unwrap_or(EngineError {
                code: "ENGINE".to_string(),
                message: "unknown engine error".to_string(),
            });
            bail!("{}: {}", err.code, err.message);
        }
        Ok(Resp::decode(response.payload.as_slice())?)
    }
}
