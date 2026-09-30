//! Sync Unix-socket RPC client. Shared by `quota`, `quota-ctl`, and tests.
//!
//! No HTTP. The daemon is the only process that talks to providers.

use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use thiserror::Error;

use crate::framing::{read_frame, write_frame};
use crate::protocol::{Request, Response};

#[derive(Debug, Error)]
pub enum RpcError {
    #[error("cannot connect: {0}")]
    Connect(String),
    #[error("{0}")]
    Io(String),
    #[error("{0}")]
    Rpc(String),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

pub fn rpc(
    socket: &Path,
    id: u64,
    method: &str,
    params: impl serde::Serialize,
) -> Result<Response, RpcError> {
    let mut stream = UnixStream::connect(socket).map_err(|e| RpcError::Connect(e.to_string()))?;
    let req = Request::with_params(id, method, params);
    let bytes = serde_json::to_vec(&req)?;
    write_frame(&mut stream, &bytes).map_err(|e| RpcError::Io(e.to_string()))?;
    let payload = read_frame(&mut stream).map_err(|e| RpcError::Io(e.to_string()))?;
    let resp: Response = serde_json::from_slice(&payload)?;
    Ok(resp)
}

/// [`rpc`] with a wall-clock budget for the whole exchange, connect included.
///
/// The exchange runs on a helper thread and the caller waits at most `budget`;
/// a daemon that is wedged, absent, or slow costs the caller `budget` and no
/// more. Use it where the caller must never hang on `quotad` (a statusline).
pub fn rpc_within(
    socket: &Path,
    id: u64,
    method: &str,
    params: impl serde::Serialize,
    budget: Duration,
) -> Result<Response, RpcError> {
    let bytes = serde_json::to_vec(&Request::with_params(id, method, params))?;
    let socket = socket.to_path_buf();
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = send.send(exchange(&socket, &bytes, budget));
    });
    receive
        .recv_timeout(budget)
        .map_err(|_| RpcError::Io(format!("no reply within {}ms", budget.as_millis())))?
}

fn exchange(socket: &Path, request: &[u8], budget: Duration) -> Result<Response, RpcError> {
    let mut stream = UnixStream::connect(socket).map_err(|e| RpcError::Connect(e.to_string()))?;
    stream
        .set_read_timeout(Some(budget))
        .and_then(|()| stream.set_write_timeout(Some(budget)))
        .map_err(|e| RpcError::Io(e.to_string()))?;
    write_frame(&mut stream, request).map_err(|e| RpcError::Io(e.to_string()))?;
    let payload = read_frame(&mut stream).map_err(|e| RpcError::Io(e.to_string()))?;
    Ok(serde_json::from_slice(&payload)?)
}

pub fn rpc_watch(
    socket: &Path,
    id: u64,
    method: &str,
    params: impl serde::Serialize,
    mut on_resp: impl FnMut(&Response),
) -> Result<(), RpcError> {
    let mut stream = UnixStream::connect(socket).map_err(|e| RpcError::Connect(e.to_string()))?;
    let req = Request::with_params(id, method, params);
    let bytes = serde_json::to_vec(&req)?;
    write_frame(&mut stream, &bytes).map_err(|e| RpcError::Io(e.to_string()))?;
    loop {
        let payload = match read_frame(&mut stream) {
            Ok(p) => p,
            Err(crate::FrameError::UnexpectedEof) => return Ok(()),
            Err(e) => return Err(RpcError::Io(e.to_string())),
        };
        let resp: Response = serde_json::from_slice(&payload)?;
        on_resp(&resp);
    }
}

pub fn decode_result<T: serde::de::DeserializeOwned>(resp: &Response) -> Result<T, RpcError> {
    if !resp.ok {
        return Err(RpcError::Rpc(err_msg(resp)));
    }
    let value = resp
        .result
        .clone()
        .ok_or_else(|| RpcError::Rpc("empty result".into()))?;
    serde_json::from_value(value).map_err(|e| RpcError::Rpc(e.to_string()))
}

pub fn err_msg(resp: &Response) -> String {
    resp.error
        .as_ref()
        .map(|e| format!("{}: {}", e.code, e.message))
        .unwrap_or_else(|| "request failed".into())
}
