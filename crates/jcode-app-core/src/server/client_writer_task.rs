// reference: https://docs.rs/tokio/1.49.0/src/tokio/runtime/io/scheduled_io.rs.html
// reference: https://docs.rs/tokio/1.49.0/src/tokio/io/split.rs.html
// reference: https://docs.rs/tokio/1.49.0/tokio/io/struct.BufReader.html
// reference: C:\Users\WorkShop1\.jcode\research\2026-11-15-sdk-cb-transport-stress.md
//! A single writer task owns a client connection's write half.
//!
//! # Why one task
//!
//! A Windows named pipe is ONE IO resource. Tokio keeps a single writer waker
//! slot per IO resource (`ScheduledIo::waiters.writer`, an `Option<Waker>`),
//! and it registers the waker only when the readiness bit is EMPTY
//! (`poll_readiness`, `ready.is_empty()` branch). Two tasks writing the same
//! half therefore interleave like this:
//!
//! 1. task A writes the ack; mio returns `Ok(len)` at SUBMISSION, so the
//!    kernel write is still in flight, and A's waker stays in the writer slot;
//! 2. task B polls, sees the readiness bit still set, so it does NOT register
//!    its own waker; mio then refuses the write (`WriteState::InFlight`),
//!    readiness is cleared, and the slot still holds A's waker;
//! 3. mio's completion for A's write wakes the slot, which still holds A's
//!    waker. B was never registered, so B is never woken again: the
//!    connection stalls forever.
//!
//! Measured on this host: two tasks writing one half hung 200/200 iterations,
//! one writer task hung 0/200.
//!
//! A `tokio::sync::Mutex` does NOT prevent this. It serializes the futures,
//! not the underlying overlapped write, because mio reports the write as
//! complete when it is only submitted.
//!
//! Routing every frame through one task makes exactly ONE task poll write
//! readiness per connection, so there is never a second waker to overwrite the
//! parked one.
//!
//! reference: https://docs.rs/tokio/1.49.0/src/tokio/runtime/io/scheduled_io.rs.html
// reference: https://docs.rs/tokio/1.49.0/src/tokio/io/split.rs.html
// reference: C:\Users\WorkShop1\.jcode\research\2026-11-15-sdk-cb-transport-stress.md

use super::*;
use crate::protocol::encode_event;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

/// A frame queued for the connection's writer task.
pub(super) type ClientFrame = Vec<u8>;

/// Handle used by every producer of client frames.
///
/// Cloning it is cheap and safe: all clones feed the same writer task, so the
/// socket is only ever polled by one task.
#[derive(Clone)]
pub(super) struct ClientWriter {
    tx: mpsc::UnboundedSender<ClientFrame>,
}

impl ClientWriter {
    /// Queue an already encoded frame.
    ///
    /// The frame is queued, not written: the writer task owns the socket.
    pub(super) fn send_frame(&self, frame: ClientFrame) -> Result<()> {
        self.tx
            .send(frame)
            .map_err(|_| anyhow::anyhow!("client connection closed"))
    }

    /// Queue several frames as ONE socket write.
    ///
    /// Frames that must not be interleaved with another producer's frames
    /// (for example a turn state followed by the history that belongs to it)
    /// go through here, so the reader sees them back to back.
    pub(super) fn send_frames(&self, frames: &[ClientFrame]) -> Result<()> {
        if frames.is_empty() {
            return Ok(());
        }
        let mut joined = Vec::with_capacity(frames.iter().map(|frame| frame.len()).sum());
        for frame in frames {
            joined.extend_from_slice(frame);
        }
        self.send_frame(joined)
    }

    /// Queue an event using the shared wire framing.
    pub(super) fn send_event(&self, event: &ServerEvent) -> Result<()> {
        self.send_frame(encode_event(event).into_bytes())
    }

    /// Whether the writer task is still alive.
    pub(super) fn is_open(&self) -> bool {
        !self.tx.is_closed()
    }
}

/// Start the writer task for a client connection.
///
/// The task owns the write half outright. It ends when the connection drops
/// or every sender is dropped.
pub(super) fn spawn_client_writer(
    mut writer: crate::transport::WriteHalf,
) -> (ClientWriter, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::unbounded_channel::<ClientFrame>();
    let handle = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            let result = writer.write_all(&frame).await;
            if let Err(error) = result {
                // Routine on client reload/disconnect; avoid dumping the full
                // frame (an await response can embed whole completion reports).
                let frame_desc =
                    crate::logging::truncate_for_log(&String::from_utf8_lossy(&frame), 200);
                crate::logging::warn(&format!(
                    "client writer failed while sending {}: {}",
                    frame_desc, error
                ));
                break;
            }
        }
    });
    (ClientWriter { tx }, handle)
}

/// Start the writer task for a client connection whose peer is a test.
///
/// The test keeps the read half, so the frames it queues are observable.
#[cfg(test)]
pub(super) fn spawn_test_client_writer(
    writer: crate::transport::WriteHalf,
) -> (ClientWriter, tokio::task::JoinHandle<()>) {
    spawn_client_writer(writer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn queued_frames_reach_the_socket_in_order() {
        let (server, client) = crate::transport::Stream::pair().expect("socket pair");
        let (client_reader, _client_writer) = client.into_split();
        // The transport ReadHalf is an AsyncRead, not an AsyncBufRead, so the
        // line reader needs a BufReader on top of it.
        let mut client_reader = tokio::io::BufReader::new(client_reader);
        let (writer, handle) = spawn_client_writer(server.into_split().1);
        writer.send_event(&ServerEvent::Ack { id: 7 }).unwrap();
        writer.send_event(&ServerEvent::Ack { id: 8 }).unwrap();
        let mut line = String::new();
        tokio::io::AsyncBufReadExt::read_line(&mut client_reader, &mut line)
            .await
            .unwrap();
        assert!(line.contains("\"id\":7"), "{line}");
        let mut line = String::new();
        tokio::io::AsyncBufReadExt::read_line(&mut client_reader, &mut line)
            .await
            .unwrap();
        assert!(line.contains("\"id\":8"), "{line}");
        drop(writer);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn a_batch_of_frames_is_written_without_interleaving() {
        let (server, client) = crate::transport::Stream::pair().expect("socket pair");
        let (client_reader, _client_writer) = client.into_split();
        let mut client_reader = tokio::io::BufReader::new(client_reader);
        let (writer, handle) = spawn_client_writer(server.into_split().1);
        writer
            .send_frames(&[
                encode_event(&ServerEvent::Ack { id: 1 }).into_bytes(),
                encode_event(&ServerEvent::Ack { id: 2 }).into_bytes(),
            ])
            .unwrap();
        let mut line = String::new();
        tokio::io::AsyncBufReadExt::read_line(&mut client_reader, &mut line)
            .await
            .unwrap();
        assert!(line.contains("\"id\":1"), "{line}");
        let mut line = String::new();
        tokio::io::AsyncBufReadExt::read_line(&mut client_reader, &mut line)
            .await
            .unwrap();
        assert!(line.contains("\"id\":2"), "{line}");
        drop(writer);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn a_closed_connection_reports_the_failure_instead_of_panicking() {
        let (server, client) = crate::transport::Stream::pair().expect("socket pair");
        let (writer, handle) = spawn_client_writer(server.into_split().1);
        drop(client);
        // The writer task may or may not have noticed the closed peer yet, so
        // the queue may still accept a frame. What must never happen is a
        // panic: a dropped connection is a normal end of life.
        let _ = writer.send_event(&ServerEvent::Ack { id: 1 });
        drop(writer);
        handle.await.unwrap();
    }
}
