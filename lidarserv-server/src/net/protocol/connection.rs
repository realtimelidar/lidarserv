use std::io::{Cursor, ErrorKind, Read};
use std::net::SocketAddr;

use bytes::{Buf, BytesMut};
use lidarserv_common::tracy_client::span;
use log::trace;
use log::info;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::broadcast::Receiver;

use crate::net::LidarServerError;
use crate::net::protocol::messages::{DeviceType, Header};

use super::messages::Message;

pub struct Connection<Stream> {
    stream: Stream,
    peer_addr: SocketAddr,
    buffer: BytesMut,

    pub dev_type: Option<DeviceType>,
}

const HEADER_SIZE: usize = 8;
const MAGIC_NUMBER: &[u8] = "LidarServ Protocol".as_bytes();

#[derive(Error, Debug)]
#[error("The connection was closed unexpectedly.")]
struct ConnectionClosedError;

#[derive(Error, Debug)]
#[error("Protocol mismatch. The peer does not seem to speak the LidarServ protocol.")]
struct ProtocolMismatchError;

impl<Stream> Connection<Stream>
where
    Stream: AsyncRead + AsyncWrite + Unpin,
{
    pub async fn new(
        stream: Stream,
        peer_addr: SocketAddr,
        shutdown: &mut Receiver<()>,
    ) -> Result<Self, LidarServerError> {
        let mut con = Connection {
            stream,
            peer_addr,
            buffer: BytesMut::new(),

            dev_type: None
        };
        con.write_magic_number().await?;
        con.read_magic_number(shutdown).await?;
        Ok(con)
    }
}

impl<Stream> Connection<Stream>
where
    Stream: AsyncRead + Unpin,
{
    async fn read_magic_number(
        &mut self,
        shutdown: &mut Receiver<()>,
    ) -> Result<(), LidarServerError> {
        // receive magic number
        let mut read_buffer = vec![0; MAGIC_NUMBER.len()];
        tokio::select! {
            result = self.stream.read_exact(read_buffer.as_mut_slice()) => match result {
                Ok(_) => {}
                Err(e) => {
                    if e.kind() == ErrorKind::UnexpectedEof {
                        return Err(LidarServerError::WireProtocol(Box::new(e)));
                    } else {
                        return Err(LidarServerError::Net(e));
                    }
                }
            },
            _ = shutdown.recv() => return Err(LidarServerError::ServerShutdown),
        } // check received bytes
        if MAGIC_NUMBER != read_buffer {
            return Err(LidarServerError::WireProtocol(Box::new(
                ProtocolMismatchError,
            )));
        }

        Ok(())
    }

    fn try_read_frame(&mut self) -> Option<Result<Message, LidarServerError>> {
        // header: just a single u64, indicating the length of the full message.
        let len = if self.buffer.len() >= HEADER_SIZE {
            let mut len_bytes = [0_u8; HEADER_SIZE];
            len_bytes.copy_from_slice(&self.buffer[..HEADER_SIZE]);
            u64::from_le_bytes(len_bytes) as usize
        } else {
            return None;
        };

        // remaining bytes: raw message data
        if self.buffer.len() < len {
            return None;
        }
        let mut data = &self.buffer[HEADER_SIZE..len];

        let message: Result<Header, _> = match self.dev_type {
            Some(DeviceType::CaptureDevice) | None => {
                ciborium::de::from_reader(&mut data)
                    .map_err(|e| LidarServerError::WireProtocol(Box::new(e)))
            },

            Some(DeviceType::Viewer) => {
                serde_json::from_reader(&mut data).map_err(|e| LidarServerError::WireProtocol(Box::new(e)))
            }
        };

        // parse cbor message
        // let message: Result<Header, _> = ciborium::de::from_reader(&mut data)
        //     .map_err(|e| LidarServerError::WireProtocol(Box::new(e)));
        // let message: Result<Header, _> = serde_json::from_reader(&mut data).map_err(|e| LidarServerError::WireProtocol(Box::new(e)));
        let payload = data.to_vec();

        // pop message of the buffer
        self.buffer.advance(len);

        // treat any message of type [Message::Error] as an error.
        trace!("{}: Receive message: {:?}", &self.peer_addr, &message);
        match message {
            Ok(Header::Error { message }) => Some(Err(LidarServerError::PeerError(message))),
            _ => Some(message.map(|m| Message { header: m, payload })),
        }
    }

    /// cancel safe
    pub async fn read_message_or_eof(
        &mut self,
        shutdown: &mut Receiver<()>,
    ) -> Result<Option<Message>, LidarServerError> {
        loop {
            let _s = span!("Connection::read_message_or_eof decode");
            if let Some(result) = self.try_read_frame() {
                return result.map(Some);
            }
            drop(_s);

            // load more data
            let bytes_read = tokio::select! {
                read_buf_result = self.stream.read_buf(&mut self.buffer) => read_buf_result?,
                _ = shutdown.recv() => return Err(LidarServerError::ServerShutdown),
            };
            if bytes_read == 0 {
                return if !self.buffer.is_empty() {
                    // connection was closed in the middle of an incoming message - this is an error
                    Err(LidarServerError::WireProtocol(Box::new(
                        ConnectionClosedError,
                    )))
                } else {
                    // connection was closed after the last message was received completely
                    Ok(None)
                };
            }
        }
    }

    pub async fn read_message(
        &mut self,
        shutdown: &mut Receiver<()>,
    ) -> Result<Message, LidarServerError> {
        match self.read_message_or_eof(shutdown).await? {
            Some(msg) => Ok(msg),
            _ => Err(LidarServerError::Protocol(format!(
                "{}",
                ConnectionClosedError
            ))),
        }
    }
}

impl<Stream> Connection<Stream>
where
    Stream: AsyncWrite + Unpin,
{
    async fn write_magic_number(&mut self) -> Result<(), LidarServerError> {
        self.stream.write_all(MAGIC_NUMBER).await?;
        Ok(())
    }

    pub async fn write_message(
        &mut self,
        header: &Header,
        payload: &[u8],
    ) -> Result<(), LidarServerError> {
        trace!("Send to {}: {:?}", &self.peer_addr, header);

        let _s2 = span!("Connection::write_message encode");
        let mut data = Vec::new();
        let header_len: usize;
        {
            let mut writer = Cursor::new(&mut data);

            // reserve space for header
            let empty_header = [0_u8; HEADER_SIZE];
            std::io::Write::write_all(&mut writer, &empty_header[..])?;

            // serialize data
            match self.dev_type {
                Some(DeviceType::CaptureDevice) | None => {
                    ciborium::ser::into_writer(header, &mut writer)
                        .map_err(|e| LidarServerError::WireProtocol(Box::new(e)))?;
                    header_len = 0;
                },
    
                Some(DeviceType::Viewer) => {
                    serde_json::to_writer(&mut writer, header).map_err(|e| LidarServerError::WireProtocol(Box::new(e)))?;
                    header_len = writer.position() as usize - 8;
                }
            };

            std::io::Write::write_all(&mut writer, payload)?;
        }

        if let Some(DeviceType::Viewer) = self.dev_type {
            if header_len > 0 {
                let v = data.to_vec();
                let str = std::str::from_utf8(&v[8..header_len+8]).expect("could not cast to utf8");
                println!("JSON Content: {}", str);
            }
        }

        // overwrite header with actual message size
        let len: u64 = {
            if let Some(DeviceType::Viewer) = self.dev_type {
                info!("Full message length: {}", data.len());
                info!("Header length: {}", header_len);
                let tmp: u64;
                tmp = (data.len() as u64 & 0xffffffff) | ((header_len as u64) << 32);
                tmp
            } else {
                data.len() as u64
            }
        };
        let len_bytes = len.to_le_bytes();
        data[0..HEADER_SIZE].copy_from_slice(&len_bytes);
        drop(_s2);

        info!("Message len: {:?}, {:?}", len, len_bytes);

        if payload.len() > 0 {
            println!("Payload bytes = ");
            for byte in payload.iter().take(18) {
                print!("{:02x} ", byte);
            }
            println!();
        }

        // send
        let _ = span!("Connection::write_message before write");
        self.stream.write_all(&data[..]).await?;
        let _ = span!("Connection::write_message after write");
        Ok(())
    }
}

impl Connection<TcpStream> {
    pub fn into_split(self) -> (Connection<OwnedReadHalf>, Connection<OwnedWriteHalf>) {
        let (read_half, write_half) = self.stream.into_split();
        (
            Connection {
                stream: read_half,
                peer_addr: self.peer_addr,
                buffer: self.buffer,
                dev_type: self.dev_type.clone(),
            },
            Connection {
                stream: write_half,
                peer_addr: self.peer_addr,
                buffer: Default::default(),
                dev_type: self.dev_type.clone(),
            },
        )
    }
}
