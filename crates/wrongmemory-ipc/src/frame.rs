//! Newline-delimited JSON framing (architecture §9): one compact JSON value
//! per line, UTF-8, `\n` terminated. The byte limit is enforced while
//! reading, before an oversized line is fully buffered.

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame exceeds the {max}-byte limit")]
    TooLarge { max: usize },
    #[error("connection closed mid-frame")]
    Truncated,
    #[error("invalid frame payload: {0}")]
    Invalid(#[from] serde_json::Error),
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
}

/// Write one frame (compact JSON + `\n`) and flush.
pub async fn write_frame<W, T>(w: &mut W, value: &T, max: usize) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
    T: Serialize + ?Sized,
{
    let mut body = serde_json::to_vec(value)?;
    if body.len() + 1 > max {
        return Err(FrameError::TooLarge { max });
    }
    body.push(b'\n');
    w.write_all(&body).await?;
    w.flush().await?;
    Ok(())
}

/// Read one raw line (without the terminator). `Ok(None)` is a clean EOF on a
/// frame boundary. Blank lines are skipped.
pub async fn read_line<R>(r: &mut R, max: usize) -> Result<Option<Vec<u8>>, FrameError>
where
    R: AsyncBufRead + Unpin,
{
    loop {
        let mut line = Vec::new();
        loop {
            let available = r.fill_buf().await?;
            if available.is_empty() {
                return if line.is_empty() {
                    Ok(None)
                } else {
                    Err(FrameError::Truncated)
                };
            }
            match available.iter().position(|b| *b == b'\n') {
                Some(i) => {
                    if line.len() + i > max {
                        return Err(FrameError::TooLarge { max });
                    }
                    line.extend_from_slice(&available[..i]);
                    r.consume(i + 1);
                    break;
                }
                None => {
                    let n = available.len();
                    if line.len() + n > max {
                        return Err(FrameError::TooLarge { max });
                    }
                    line.extend_from_slice(available);
                    r.consume(n);
                }
            }
        }
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        if line.iter().all(|b| b.is_ascii_whitespace()) {
            continue;
        }
        return Ok(Some(line));
    }
}

/// Read and decode one frame. `Ok(None)` is a clean EOF.
pub async fn read_frame<R, T>(r: &mut R, max: usize) -> Result<Option<T>, FrameError>
where
    R: AsyncBufRead + Unpin,
    T: DeserializeOwned,
{
    match read_line(r, max).await? {
        None => Ok(None),
        Some(line) => Ok(Some(serde_json::from_slice(&line)?)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use tokio::io::BufReader;

    #[tokio::test]
    async fn round_trip_and_clean_eof() {
        let (mut a, b) = tokio::io::duplex(1024);
        write_frame(&mut a, &json!({"x": "çağrı 🚀\nline"}), 1024)
            .await
            .unwrap();
        write_frame(&mut a, &json!(2), 1024).await.unwrap();
        drop(a);
        let mut b = BufReader::new(b);
        let v: Value = read_frame(&mut b, 1024).await.unwrap().unwrap();
        assert_eq!(v["x"], "çağrı 🚀\nline");
        let v: Value = read_frame(&mut b, 1024).await.unwrap().unwrap();
        assert_eq!(v, json!(2));
        assert!(
            read_frame::<_, Value>(&mut b, 1024)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn oversized_line_rejected_without_newline() {
        let (mut a, b) = tokio::io::duplex(64 * 1024);
        a.write_all(&vec![b'x'; 10_000]).await.unwrap();
        let mut b = BufReader::new(b);
        let err = read_frame::<_, Value>(&mut b, 100).await.unwrap_err();
        assert!(matches!(err, FrameError::TooLarge { max: 100 }));
    }

    #[tokio::test]
    async fn truncated_and_invalid_frames_are_errors() {
        let (mut a, b) = tokio::io::duplex(64);
        a.write_all(b"{\"a\"").await.unwrap();
        drop(a);
        let mut b = BufReader::new(b);
        assert!(matches!(
            read_frame::<_, Value>(&mut b, 100).await.unwrap_err(),
            FrameError::Truncated
        ));
        let (mut a, b) = tokio::io::duplex(64);
        a.write_all(b"not json\r\n").await.unwrap();
        drop(a);
        let mut b = BufReader::new(b);
        assert!(matches!(
            read_frame::<_, Value>(&mut b, 100).await.unwrap_err(),
            FrameError::Invalid(_)
        ));
    }

    #[tokio::test]
    async fn writer_refuses_oversized_payload() {
        let (mut a, _b) = tokio::io::duplex(64);
        let big = "x".repeat(200);
        assert!(matches!(
            write_frame(&mut a, &big, 100).await.unwrap_err(),
            FrameError::TooLarge { .. }
        ));
    }
}
