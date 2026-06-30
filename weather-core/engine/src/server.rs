use std::sync::atomic::Ordering;

use anyhow::{Context, Result};
use prost::Message;
use prost::bytes::Bytes;
use tokio::sync::broadcast;
use weather_schema::*;
use zeromq::{PubSocket, RouterSendHalf, Socket, SocketRecv, SocketSend, ZmqMessage};

use crate::runtime::{Engine, EngineExit};

pub(crate) type EventSink = broadcast::Sender<(String, EventEnvelope)>;

pub(crate) async fn run_engine_sockets(
    engine: Engine,
    rpc_endpoint: String,
    pub_endpoint: String,
    mode: String,
) -> Result<EngineExit> {
    let publisher = spawn_publisher(pub_endpoint.clone(), engine.sink.clone())
        .await
        .with_context(|| format!("failed to bind PUB endpoint {pub_endpoint}"))?;
    let router = spawn_router(
        engine.clone(),
        rpc_endpoint.clone(),
        mode.clone(),
        pub_endpoint.clone(),
    )
    .await
    .with_context(|| format!("failed to bind RPC endpoint {rpc_endpoint}"))?;

    let refresh_handle = crate::refresh::spawn_refresh_loop(engine.clone());

    engine.publish_status(&mode, &rpc_endpoint, &pub_endpoint);

    // 注册信号 handler:收到 SIGINT/SIGTERM(Unix)或 Ctrl+C(Win)后置 stop 标志,
    // 与 Shutdown RPC 走同一退出链路。
    let stop_for_signal = engine.stop.clone();
    let signal_task = tokio::spawn(async move {
        let _ = wait_for_signal().await;
        stop_for_signal.store(true, Ordering::SeqCst);
    });

    // 等 stop 标志被置(Shutdown RPC / 信号 / RestartEngine)。
    while !engine.stop.load(Ordering::SeqCst) {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    signal_task.abort();

    // graceful cleanup:abort 后台 task,join 等退出(超时兜底),db checkpoint。
    refresh_handle.abort();
    publisher.abort();
    router.abort();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let _ = refresh_handle.await;
    })
    .await;
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let _ = publisher.await;
        let _ = router.await;
    })
    .await;
    let _ = engine.db.shutdown().await;

    if engine.restart.load(Ordering::SeqCst) {
        Ok(EngineExit::Restart)
    } else {
        Ok(EngineExit::Shutdown)
    }
}

/// 等待 OS 信号:Unix 下 SIGINT/SIGTERM,Windows 下 Ctrl+C。
async fn wait_for_signal() -> Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigint = signal(SignalKind::interrupt())?;
        let mut sigterm = signal(SignalKind::terminate())?;
        tokio::select! {
            _ = sigint.recv() => {}
            _ = sigterm.recv() => {}
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await
    }
}

async fn spawn_publisher(
    pub_endpoint: String,
    sink: EventSink,
) -> Result<tokio::task::JoinHandle<()>> {
    let mut socket = PubSocket::new();
    socket
        .bind(&pub_endpoint)
        .await
        .with_context(|| format!("failed to bind PUB endpoint {pub_endpoint}"))?;
    let mut rx = sink.subscribe();
    Ok(tokio::spawn(async move {
        while let Ok((topic, event)) = rx.recv().await {
            let payload = event.encode_to_vec();
            let mut frames = std::collections::VecDeque::new();
            frames.push_back(Bytes::from(topic));
            frames.push_back(Bytes::from(payload));
            if socket
                .send(ZmqMessage::try_from(frames).expect("non-empty message"))
                .await
                .is_err()
            {
                break;
            }
        }
    }))
}

async fn spawn_router(
    engine: Engine,
    rpc_endpoint: String,
    mode: String,
    pub_endpoint: String,
) -> Result<tokio::task::JoinHandle<()>> {
    let mut socket = zeromq::RouterSocket::new();
    socket
        .bind(&rpc_endpoint)
        .await
        .with_context(|| format!("failed to bind RPC endpoint {rpc_endpoint}"))?;
    let (send_half, mut recv_half) = socket.split();
    Ok(tokio::spawn(async move {
        while let Ok(message) = recv_half.recv().await {
            let mut frames = message.into_vecdeque();
            let Some(identity) = frames.pop_front() else {
                continue;
            };
            let Some(payload) = frames.pop_front() else {
                send_back_error(
                    send_half.clone(),
                    identity,
                    "",
                    "BAD_REQUEST",
                    "missing rpc frame",
                )
                .await;
                continue;
            };
            let engine = engine.clone();
            let mode = mode.clone();
            let pub_endpoint = pub_endpoint.clone();
            let rpc_endpoint = rpc_endpoint.clone();
            let send_half = send_half.clone();
            tokio::spawn(async move {
                let response = match RpcRequest::decode(payload.as_ref()) {
                    Ok(request) => {
                        engine
                            .handle_rpc_request(request, &mode, &rpc_endpoint, &pub_endpoint)
                            .await
                    }
                    Err(err) => Engine::rpc_error_response(
                        "",
                        "BAD_REQUEST",
                        format!("invalid rpc request: {err}"),
                    ),
                };
                send_back(send_half, identity, response.encode_to_vec()).await;
            });
        }
    }))
}

async fn send_back(mut send_half: RouterSendHalf, identity: Bytes, payload: Vec<u8>) {
    let mut frames = std::collections::VecDeque::new();
    frames.push_back(identity);
    frames.push_back(Bytes::from(payload));
    let _ = send_half
        .send(ZmqMessage::try_from(frames).expect("non-empty message"))
        .await;
}

async fn send_back_error(
    send_half: RouterSendHalf,
    identity: Bytes,
    request_id: &str,
    code: &str,
    message: &str,
) {
    let response = Engine::rpc_error_response(request_id, code, message);
    send_back(send_half, identity, response.encode_to_vec()).await;
}
