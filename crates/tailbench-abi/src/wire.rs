//! Length-prefixed bincode framing, shared by both sockets.

use anyhow::Result;
use serde::{de::DeserializeOwned, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub async fn write_msg<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, msg: &T) -> Result<()> {
    let bytes = bincode::serialize(msg)?;
    w.write_all(&(bytes.len() as u32).to_le_bytes()).await?;
    w.write_all(&bytes).await?;
    w.flush().await?;
    Ok(())
}

pub async fn read_msg<R: AsyncRead + Unpin, T: DeserializeOwned>(r: &mut R) -> Result<T> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let mut buf = vec![0u8; u32::from_le_bytes(len) as usize];
    r.read_exact(&mut buf).await?;
    Ok(bincode::deserialize(&buf)?)
}
