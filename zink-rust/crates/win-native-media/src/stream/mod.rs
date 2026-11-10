//! RTMP publish-only client: connect, publish, stream H.264 video tags.
//!
//! Flow: TCP connect -> handshake -> AMF0 connect -> createStream -> publish ->
//! @setDataFrame metadata -> AVC sequence header -> per-frame video messages.
//! Reconnect uses exponential backoff; after a reconnect we resend the sequence
//! header and wait for the next keyframe before resuming frame data.

pub mod amf0;
pub mod chunk;
pub mod flv;
pub mod handshake;

use std::{sync::Arc, time::Duration};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::{
    rustls::{pki_types::ServerName, ClientConfig, RootCertStore},
    TlsConnector,
};

use crate::encoder::{EncodedSample, ParameterSets};
use crate::{PipelineError, Result, StreamConfig, VideoConfig};

use amf0::Amf0;
use chunk::{msg_type, Message};

/// Chunk stream ids (arbitrary but conventional): control on 2, commands on 3,
/// media on 4/5.
const CSID_CONTROL: u32 = 2;
const CSID_COMMAND: u32 = 3;
const CSID_VIDEO: u32 = 6;
const CSID_AUDIO: u32 = 7;
const CSID_DATA: u32 = 4;

/// Our outbound chunk size. Larger than the default 128 to cut per-chunk
/// overhead for video; we announce it via Set Chunk Size right after connect.
const OUT_CHUNK_SIZE: usize = 4096;

trait RtmpIo: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T> RtmpIo for T where T: AsyncRead + AsyncWrite + Unpin + Send {}

pub struct RtmpPublisher {
    stream: Box<dyn RtmpIo>,
    cfg: StreamConfig,
    video: VideoConfig,
    params: ParameterSets,
    msg_stream_id: u32,
    /// The server's chunk size for messages it sends us (default 128 until it
    /// sends a Set Chunk Size). Must be honored or inbound reassembly desyncs.
    in_chunk_size: usize,
    seq_header_sent: bool,
    /// After a reconnect we must wait for a keyframe before sending frames.
    awaiting_keyframe: bool,
    transaction_id: f64,
    out: Vec<u8>,
}

impl RtmpPublisher {
    /// Connect and complete the publish handshake. `params` are the SPS/PPS for
    /// the AVC sequence header (must be non-empty).
    pub async fn connect(
        cfg: StreamConfig,
        video: VideoConfig,
        params: ParameterSets,
    ) -> Result<Self> {
        let (host, port, app, secure) = parse_rtmp_url(&cfg.url)?;
        let tcp = TcpStream::connect((host.as_str(), port)).await?;
        tcp.set_nodelay(true).ok();
        let mut stream: Box<dyn RtmpIo> = if secure {
            let native = rustls_native_certs::load_native_certs();
            let mut roots = RootCertStore::empty();
            let (added, ignored) = roots.add_parsable_certificates(native.certs);
            if added == 0 {
                return Err(PipelineError::Rtmp(format!(
                    "no trusted Windows certificates were available for {host} ({ignored} ignored)"
                )));
            }
            let config = ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            let server_name = ServerName::try_from(host.clone()).map_err(|error| {
                PipelineError::Rtmp(format!("invalid RTMPS server name {host}: {error}"))
            })?;
            let tls = TlsConnector::from(Arc::new(config))
                .connect(server_name, tcp)
                .await
                .map_err(|error| {
                    PipelineError::Rtmp(format!("TLS connection to {host} failed: {error}"))
                })?;
            Box::new(tls)
        } else {
            Box::new(tcp)
        };

        handshake::handshake(&mut stream).await?;

        let mut me = Self {
            stream,
            cfg: cfg.clone(),
            video,
            params,
            msg_stream_id: 0,
            in_chunk_size: 128,
            seq_header_sent: false,
            awaiting_keyframe: false,
            transaction_id: 1.0,
            out: Vec::with_capacity(64 * 1024),
        };

        // Announce our chunk size.
        me.send_control(
            msg_type::SET_CHUNK_SIZE,
            chunk::set_chunk_size_payload(OUT_CHUNK_SIZE as u32),
        )
        .await?;

        me.send_connect(&app).await?;
        me.expect_result("connect").await?;

        me.msg_stream_id = me.send_create_stream().await?;
        me.send_publish().await?;
        // Servers reply onStatus NetStream.Publish.Start; we read/drain until we
        // see a reply, but don't strictly require parsing it.
        me.drain_one_reply().await?;

        me.send_metadata().await?;
        me.send_sequence_header().await?;

        Ok(me)
    }

    /// Send the AAC sequence header (once) before any audio frames. Call after
    /// connect if streaming audio. `asc` is the AudioSpecificConfig.
    pub async fn send_audio_sequence_header(&mut self, asc: &[u8]) -> Result<()> {
        let body = flv::aac_sequence_header(asc);
        self.send_media(CSID_AUDIO, msg_type::AUDIO, 0, body).await
    }

    /// Send one encoded AAC frame at `timestamp`.
    pub async fn send_audio(&mut self, aac: &[u8], timestamp: Duration) -> Result<()> {
        let ts = flv::duration_to_flv_ms(timestamp);
        let body = flv::aac_frame(aac);
        self.send_media(CSID_AUDIO, msg_type::AUDIO, ts, body).await
    }

    /// Send one encoded video sample. Skips frames until the first keyframe
    /// after (re)connect so the decoder can sync.
    pub async fn send_video(&mut self, sample: &EncodedSample) -> Result<()> {
        if self.awaiting_keyframe {
            if !sample.is_keyframe {
                return Ok(());
            }
            self.awaiting_keyframe = false;
        }
        let ts = flv::duration_to_flv_ms(sample.timestamp);
        let body = flv::avc_frame(&sample.data, sample.is_keyframe);
        self.send_media(CSID_VIDEO, msg_type::VIDEO, ts, body).await
    }

    /// Reconnect after a drop: rebuild the session and require a keyframe.
    pub async fn reconnect(&mut self) -> Result<()> {
        let mut backoff = Duration::from_millis(500);
        let max = Duration::from_secs(10);
        loop {
            match Self::connect(self.cfg.clone(), self.video, self.params.clone()).await {
                Ok(fresh) => {
                    *self = fresh;
                    self.awaiting_keyframe = true;
                    return Ok(());
                }
                Err(e) => {
                    tracing::warn!("rtmp reconnect failed: {e}; retrying in {backoff:?}");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(max);
                }
            }
        }
    }

    // --- command senders -------------------------------------------------

    async fn send_connect(&mut self, app: &str) -> Result<()> {
        let tx = self.next_txn();
        let tc_url = self.cfg.url.clone();
        let cmd = amf0::encode_values(&[
            Amf0::String("connect".into()),
            Amf0::Number(tx),
            Amf0::Object(vec![
                ("app".into(), Amf0::String(app.into())),
                ("type".into(), Amf0::String("nonprivate".into())),
                ("flashVer".into(), Amf0::String("FMLE/3.0".into())),
                ("tcUrl".into(), Amf0::String(tc_url)),
            ]),
        ]);
        self.send_command(cmd).await
    }

    async fn send_create_stream(&mut self) -> Result<u32> {
        let tx = self.next_txn();
        let cmd = amf0::encode_values(&[
            Amf0::String("createStream".into()),
            Amf0::Number(tx),
            Amf0::Null,
        ]);
        self.send_command(cmd).await?;
        // Read the _result; its trailing number is the message stream id.
        let reply = self.read_command_reply().await?;
        let id = reply.last_number.unwrap_or(1.0);
        Ok(id as u32)
    }

    async fn send_publish(&mut self) -> Result<()> {
        let tx = self.next_txn();
        let cmd = amf0::encode_values(&[
            Amf0::String("publish".into()),
            Amf0::Number(tx),
            Amf0::Null,
            Amf0::String(self.cfg.stream_key.clone()),
            Amf0::String("live".into()),
        ]);
        // publish command goes on the message stream we just created.
        self.send_command_on_stream(cmd, self.msg_stream_id).await
    }

    async fn send_metadata(&mut self) -> Result<()> {
        // @setDataFrame(onMetaData, {...})
        let meta = amf0::encode_values(&[
            Amf0::String("@setDataFrame".into()),
            Amf0::String("onMetaData".into()),
            Amf0::Object(vec![
                ("width".into(), Amf0::Number(self.video.width as f64)),
                ("height".into(), Amf0::Number(self.video.height as f64)),
                ("framerate".into(), Amf0::Number(self.video.fps as f64)),
                ("videocodecid".into(), Amf0::Number(7.0)), // AVC
                (
                    "videodatarate".into(),
                    Amf0::Number(self.video.bitrate as f64 / 1000.0),
                ),
            ]),
        ]);
        let msg = Message {
            chunk_stream_id: CSID_DATA,
            timestamp: 0,
            type_id: msg_type::DATA_AMF0,
            msg_stream_id: self.msg_stream_id,
            payload: meta,
        };
        self.write_msg(msg).await
    }

    async fn send_sequence_header(&mut self) -> Result<()> {
        let body = flv::avc_sequence_header(&self.params);
        self.send_media(CSID_VIDEO, msg_type::VIDEO, 0, body)
            .await?;
        self.seq_header_sent = true;
        Ok(())
    }

    // --- low-level send helpers -----------------------------------------

    async fn send_command(&mut self, payload: Vec<u8>) -> Result<()> {
        self.send_command_on_stream(payload, 0).await
    }

    async fn send_command_on_stream(&mut self, payload: Vec<u8>, msid: u32) -> Result<()> {
        let msg = Message {
            chunk_stream_id: CSID_COMMAND,
            timestamp: 0,
            type_id: msg_type::COMMAND_AMF0,
            msg_stream_id: msid,
            payload,
        };
        self.write_msg(msg).await
    }

    async fn send_control(&mut self, type_id: u8, payload: Vec<u8>) -> Result<()> {
        let msg = Message {
            chunk_stream_id: CSID_CONTROL,
            timestamp: 0,
            type_id,
            msg_stream_id: 0,
            payload,
        };
        self.write_msg(msg).await
    }

    async fn send_media(
        &mut self,
        csid: u32,
        type_id: u8,
        ts: u32,
        payload: Vec<u8>,
    ) -> Result<()> {
        let msg = Message {
            chunk_stream_id: csid,
            timestamp: ts,
            type_id,
            msg_stream_id: self.msg_stream_id,
            payload,
        };
        self.write_msg(msg).await
    }

    async fn write_msg(&mut self, msg: Message) -> Result<()> {
        self.out.clear();
        chunk::write_message(&mut self.out, &msg, OUT_CHUNK_SIZE);
        if std::env::var("RTMP_DEBUG").is_ok() {
            eprintln!(
                "[rtmp] send type={} csid={} len={} first16={:02x?}",
                msg.type_id,
                msg.chunk_stream_id,
                msg.payload.len(),
                &self.out[..self.out.len().min(16)]
            );
        }
        self.stream.write_all(&self.out).await?;
        Ok(())
    }

    // --- reply reading ---------------------------------------------------

    /// Read chunks until we assemble one command message, returning its parsed
    /// reply. Control messages (window ack, set peer bw, etc.) are skipped.
    async fn read_command_reply(&mut self) -> Result<amf0::Amf0CommandReply> {
        loop {
            let (type_id, payload) = self.read_one_message().await?;
            if type_id == msg_type::COMMAND_AMF0 {
                if let Some(reply) = amf0::parse_command_reply(&payload) {
                    return Ok(reply);
                }
            }
            // else: control/other message, keep reading.
        }
    }

    async fn expect_result(&mut self, ctx: &str) -> Result<()> {
        let reply = self.read_command_reply().await?;
        if reply.command == "_error" {
            return Err(PipelineError::Rtmp(format!(
                "{ctx}: server returned _error"
            )));
        }
        Ok(())
    }

    async fn drain_one_reply(&mut self) -> Result<()> {
        // Best-effort: read one message (the publish onStatus). Ignore parse.
        let _ = self.read_one_message().await?;
        Ok(())
    }

    /// Read and reassemble a single RTMP message from the socket. Handles the
    /// chunk basic/message headers for the formats a server sends us (type 0
    /// and type 3 continuations), respecting the server's chunk size (assumed
    /// default 128 for inbound — we never raise the server's chunk size).
    async fn read_one_message(&mut self) -> Result<(u8, Vec<u8>)> {
        // Basic header.
        let b0 = self.read_u8().await?;
        let fmt = b0 >> 6;
        let csid = (b0 & 0x3f) as u32; // (2/3-byte csid forms are rare from servers)
        if std::env::var("RTMP_DEBUG").is_ok() {
            eprintln!("[rtmp] recv basic byte {b0:#04x} fmt={fmt} csid={csid}");
        }

        // Message header by fmt.
        let (mut msg_len, type_id) = match fmt {
            0 => {
                let _ts = self.read_n(3).await?;
                let len = be_u24(&self.read_n(3).await?);
                let tid = self.read_u8().await?;
                let _msid = self.read_n(4).await?;
                (len as usize, tid)
            }
            1 => {
                let _ts = self.read_n(3).await?;
                let len = be_u24(&self.read_n(3).await?);
                let tid = self.read_u8().await?;
                (len as usize, tid)
            }
            _ => {
                // type 2/3 without prior state: we can't size it. Treat as 0-len
                // control we skip. This is sufficient for our request/reply flow
                // where servers use fmt 0 for the messages we care about.
                (0usize, 0u8)
            }
        };

        if std::env::var("RTMP_DEBUG").is_ok() {
            eprintln!("[rtmp]   -> fmt={fmt} type={type_id} len={msg_len}");
        }
        let mut payload = Vec::with_capacity(msg_len);
        while payload.len() < msg_len {
            let want = (msg_len - payload.len()).min(self.in_chunk_size);
            let chunk = self.read_n(want).await?;
            payload.extend_from_slice(&chunk);
            if payload.len() < msg_len {
                // Next continuation chunk starts with a type-3 basic header byte.
                let _cont = self.read_u8().await?;
            }
        }
        let _ = &mut msg_len;
        let _ = csid;

        // Honor the server's Set Chunk Size so subsequent reassembly stays aligned.
        if type_id == msg_type::SET_CHUNK_SIZE && payload.len() >= 4 {
            let sz = be_u24(&payload[1..4]) | ((payload[0] as u32 & 0x7f) << 24);
            if sz > 0 {
                self.in_chunk_size = sz as usize;
            }
        }
        Ok((type_id, payload))
    }

    async fn read_u8(&mut self) -> Result<u8> {
        Ok(self.stream.read_u8().await?)
    }
    async fn read_n(&mut self, n: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; n];
        self.stream.read_exact(&mut buf).await?;
        Ok(buf)
    }

    fn next_txn(&mut self) -> f64 {
        let t = self.transaction_id;
        self.transaction_id += 1.0;
        t
    }
}

fn be_u24(b: &[u8]) -> u32 {
    ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32
}

/// Parse rtmp[s]://host[:port]/app from the URL.
/// Returns (host, port, app, secure).
fn parse_rtmp_url(url: &str) -> Result<(String, u16, String, bool)> {
    let (rest, secure) = if let Some(rest) = url.strip_prefix("rtmps://") {
        (rest, true)
    } else if let Some(rest) = url.strip_prefix("rtmp://") {
        (rest, false)
    } else {
        return Err(PipelineError::Rtmp(format!(
            "not an RTMP or RTMPS url: {url}"
        )));
    };
    let (authority, app) = match rest.split_once('/') {
        Some((a, p)) => (a, p.trim_end_matches('/')),
        None => (rest, ""),
    };
    let (host, port) = match authority.split_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse()
                .map_err(|_| PipelineError::Rtmp(format!("bad port in {url}")))?,
        ),
        None => (authority.to_string(), if secure { 443 } else { 1935 }),
    };
    if app.is_empty() {
        return Err(PipelineError::Rtmp(format!("no app in rtmp url: {url}")));
    }
    Ok((host, port, app.to_string(), secure))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rtmp_url() {
        let (h, p, a, secure) = parse_rtmp_url("rtmp://localhost/live").unwrap();
        assert_eq!(
            (h.as_str(), p, a.as_str(), secure),
            ("localhost", 1935, "live", false)
        );
        let (h, p, a, secure) = parse_rtmp_url("rtmp://1.2.3.4:1936/app/sub").unwrap();
        assert_eq!(
            (h.as_str(), p, a.as_str(), secure),
            ("1.2.3.4", 1936, "app/sub", false)
        );
        let (h, p, a, secure) = parse_rtmp_url("rtmps://live.example.com/app").unwrap();
        assert_eq!(
            (h.as_str(), p, a.as_str(), secure),
            ("live.example.com", 443, "app", true)
        );
    }

    #[test]
    fn rejects_non_rtmp() {
        assert!(parse_rtmp_url("http://x/y").is_err());
        assert!(parse_rtmp_url("rtmp://host").is_err());
    }
}
