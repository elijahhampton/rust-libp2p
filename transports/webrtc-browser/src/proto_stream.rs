use std::marker::PhantomData;

use futures::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use prost::Message;

use crate::error::Error;

/// A wrapper around a Stream enabling reads and writes for protobuf messages.

pub struct ProtobufStream<M, S>
where
    S: AsyncRead + AsyncWrite + Unpin + 'static,
{
    stream: S,
    _phantom: PhantomData<M>,
}

impl<M, S> ProtobufStream<M, S>
where
    M: Message + Default,
    S: AsyncRead + AsyncWrite + Unpin + 'static,
{
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            _phantom: PhantomData,
        }
    }

    /// Read a series of bytes from the underlying [`Stream`].
    pub async fn read(&mut self) -> Result<M, Error> {
        let mut len_bytes = [0u8; 4];
        self.stream.read_exact(&mut len_bytes).await?;
        let len = u32::from_be_bytes(len_bytes) as usize;

        let mut buf = vec![0u8; len];
        self.stream.read_exact(&mut buf).await?;

        M::decode(&buf[..]).map_err(|e| Error::ProtoSerialization(e.to_string()))
    }

    /// Write a series of bytes to the underlying [`Stream`].
    pub async fn write(&mut self, message: M) -> Result<(), Error> {
        let len = message.encoded_len();
        let len_bytes = (len as u32).to_be_bytes();

        let mut buf = Vec::with_capacity(len);
        message
            .encode(&mut buf)
            .map_err(|e| Error::ProtoSerialization(e.to_string()))?;

        self.stream.write_all(&len_bytes).await?;
        self.stream.write_all(&buf).await?;
        self.stream.flush().await?;

        Ok(())
    }
}
