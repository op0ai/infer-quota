use std::os::unix::net::UnixStream;
use std::path::Path;

use quota_core::framing::{read_frame, write_frame};
use quota_core::protocol::{Request, Response};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ClientError {
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
) -> Result<Response, ClientError> {
    let mut stream =
        UnixStream::connect(socket).map_err(|e| ClientError::Connect(e.to_string()))?;
    let req = Request::with_params(id, method, params);
    let bytes = serde_json::to_vec(&req)?;
    write_frame(&mut stream, &bytes).map_err(|e| ClientError::Io(e.to_string()))?;
    let payload = read_frame(&mut stream).map_err(|e| ClientError::Io(e.to_string()))?;
    let resp: Response = serde_json::from_slice(&payload)?;
    Ok(resp)
}

pub fn rpc_watch(
    socket: &Path,
    id: u64,
    method: &str,
    params: impl serde::Serialize,
    mut on_resp: impl FnMut(&Response),
) -> Result<(), ClientError> {
    let mut stream =
        UnixStream::connect(socket).map_err(|e| ClientError::Connect(e.to_string()))?;
    let req = Request::with_params(id, method, params);
    let bytes = serde_json::to_vec(&req)?;
    write_frame(&mut stream, &bytes).map_err(|e| ClientError::Io(e.to_string()))?;
    loop {
        let payload = match read_frame(&mut stream) {
            Ok(p) => p,
            Err(quota_core::FrameError::UnexpectedEof) => return Ok(()),
            Err(e) => return Err(ClientError::Io(e.to_string())),
        };
        let resp: Response = serde_json::from_slice(&payload)?;
        on_resp(&resp);
    }
}
