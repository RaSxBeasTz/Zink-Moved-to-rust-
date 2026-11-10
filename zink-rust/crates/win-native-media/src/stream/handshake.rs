//! RTMP simple handshake (C0/C1/C2 <-> S0/S1/S2).
//!
//! We use the plain (non-crypto-digest) handshake: it interoperates with
//! mediamtx, nginx-rtmp, and the major ingest servers for publishing. C1/C2 are
//! 1536 bytes: 4-byte time, 4-byte zero, 1528 random bytes. We echo S1 as C2.

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::Result;

const RTMP_VERSION: u8 = 3;
const HANDSHAKE_SIZE: usize = 1536;

pub async fn handshake<S>(stream: &mut S) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + ?Sized,
{
    // --- send C0 + C1 ---
    let mut c0c1 = Vec::with_capacity(1 + HANDSHAKE_SIZE);
    c0c1.push(RTMP_VERSION); // C0
                             // C1: time(4)=0, zero(4)=0, then random.
    c0c1.extend_from_slice(&[0u8; 8]);
    let mut rng = SimpleRng::new();
    c0c1.extend((0..HANDSHAKE_SIZE - 8).map(|_| rng.next_u8()));
    stream.write_all(&c0c1).await?;

    // --- read S0 + S1 + S2 ---
    let mut s0 = [0u8; 1];
    stream.read_exact(&mut s0).await?;
    // Some servers send v3; don't hard-fail on mismatch, just proceed.

    let mut s1 = [0u8; HANDSHAKE_SIZE];
    stream.read_exact(&mut s1).await?;

    let mut s2 = [0u8; HANDSHAKE_SIZE];
    stream.read_exact(&mut s2).await?;

    // --- send C2 = echo of S1 ---
    stream.write_all(&s1).await?;

    Ok(())
}

/// Tiny xorshift RNG — the handshake random bytes don't need crypto quality.
struct SimpleRng(u64);

impl SimpleRng {
    fn new() -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9)
            | 1;
        SimpleRng(seed)
    }
    fn next_u8(&mut self) -> u8 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        (x >> 24) as u8
    }
}
