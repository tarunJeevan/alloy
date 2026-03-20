//! Background preview render worker.
//!
//! Runs on a dedicated `std::thread`. The UI thread sends `RenderRequest`s via a bounded `SyncSender`. The worker debounces, renders, and sends `RenderResult`s back.
//!
//! Debounce algorithm (recv_timeout):
//! 1. Block on `recv()` waiting for the first request.
//! 2. Enter the drain loop: call `recv_timeout(debounce_ms)`.
//!   - If a newer request arrives before the timeout, replace current request loop.
//!   - If the timeout fires (no new request within the debounce window), exit drain loop and render the most recent request.
//! 3. Send the `RenderResult` back via `result_sender`.
//! 4. Go to step 1.
//!
//! Panic safety - The worker body is wrapped in `std::panic::catch_unwind`. On panic, a sentinel `RenderResult` is sent to the UI so the preview pane shows an error message rather than silently going stale.
//!
//! Channel sizing - The request channel is BOUNDED (`sync_channel(4)`). The UI uses `try_send` and silently drops if the channel is full. The next keystroke will trigger a fresh request anyway. This ensures the worker is never starved of CPU by a backlog of stale render jobs.

use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::Duration;

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
};

// ---------------------------------------------------------
// Public channel types
// ---------------------------------------------------------

/// A render request sent from the UI thread to the worker.
#[derive(Debug, Clone)]
pub struct RenderRequest {
    /// Monotonically increasing document revision counter.
    /// The UI discards results whose revision doesn't match the current counter.
    pub revision: u64,

    /// The full Markdown source text to render
    pub markdown: String,

    /// Width of the preview pane in terminal columns.
    /// Used by the real renderer (Chunk 3.2) for line-wrapping.
    pub col_width: u16,
}

/// A render result sent from the worker back to the UI thread.
#[derive(Debug)]
pub struct RenderResult {
    /// The revision this result corresponds to.
    pub revision: u64,

    /// The rendered content to display in the preview pane.
    pub text: Text<'static>,
}

// ---------------------------------------------------------
// Channel constructor
// ---------------------------------------------------------

/// Create the request/result channel pair and spawn the render worker thread.
///
/// Returns `(request_sender, result_receiver, thread_handle)`.
/// The `JoinHandle` can be stored on `App` or dropped - the worker exits cleanly when the `request_sender` is dropped (i.e. when `App` is dropped).
pub fn spawn_worker(
    debounce_ms: u64,
) -> (
    SyncSender<RenderRequest>,
    Receiver<RenderResult>,
    std::thread::JoinHandle<()>,
) {
    // Bounded request channel with a capacity of 4.
    // The UI uses try_send() and drops if full.
    let (req_tx, req_rx) = sync_channel::<RenderRequest>(4);

    // Unbounded result channel - the worker sends at most one result per render cycle so there's no risk of backlog.
    let (res_tx, res_rx) = std::sync::mpsc::channel::<RenderResult>();

    let handle = std::thread::Builder::new()
        .name("preview-worker".into())
        .spawn(move || {
            worker_loop(req_rx, res_tx, Duration::from_millis(debounce_ms));
        })
        .expect("failed to spawn preview worker thread");

    (req_tx, res_rx, handle)
}

// ---------------------------------------------------------
// Worker loop
// ---------------------------------------------------------

fn worker_loop(
    req_rx: Receiver<RenderRequest>,
    res_tx: std::sync::mpsc::Sender<RenderResult>,
    debounce: Duration,
) {
    loop {
        // Block until the first request arrives (or the channel is closed, in which case exit).
        let first = match req_rx.recv() {
            Ok(r) => r,
            Err(_) => {
                tracing::debug!("preview worker: request channel closed, exiting");
                return;
            }
        };

        // Drain loop - keep replacing `current` with newer requests until the debounce window expires with no new arrivals.
        let current = drain_to_latest(first, &req_rx, debounce);

        // Render inside catch_unwind so a panic in the renderer doesn't kill the worker.
        let revision = current.revision;
        let result = std::panic::catch_unwind(|| render_stub(&current));

        let text = match result {
            Ok(t) => t,
            Err(_) => {
                tracing::error!("preview worker: renderer panicked for revision {revision}");
                error_text("Preview renderer panicked. Chack logs for more information.")
            }
        };

        // Send the result. If the UI has dropped its receiver (app is shutting down), exit.
        if res_tx.send(RenderResult { revision, text }).is_err() {
            tracing::debug!("preview worker: result channel closed. Exiting...");
            return;
        }
    }
}

/// Drain the request channel until a `recv_timeout` fires (no new request within `debounce`).
///
/// Returns the most-recent request.
fn drain_to_latest(
    mut current: RenderRequest,
    req_rx: &Receiver<RenderRequest>,
    debounce: Duration,
) -> RenderRequest {
    loop {
        match req_rx.recv_timeout(debounce) {
            Ok(newer) => {
                // A newer request arrived before the timeout expired - replace and reset the window.
                current = newer;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Debounce window expired with no new arrivals - this is the request to render.
                return current;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                // Channel closed mid-drain (app is shutting down) - return what we have.
                return current;
            }
        }
    }
}

// ---------------------------------------------------------
// Stub renderer (Real renderer implemented in Chunk 3.2)
// ---------------------------------------------------------

/// Placeholder renderer: converts the Markdown source to plain `Text` line-by-line.
///
/// This is intentionally minimal - it exists only to verify the channel plumbing works end-to-end before Chunk 3.2 replaces it with the real `pulldown-cmark` renderer.
fn render_stub(req: &RenderRequest) -> Text<'static> {
    let header = Line::from(vec![
        Span::styled(
            "[ Preview ]",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        ),
        Span::styled(
            "(stub - real renderer in Chunk 3.2)",
            Style::default().fg(Color::DarkGray),
        ),
    ]);

    let separator = Line::from(Span::styled(
        "-".repeat(req.col_width.max(1) as usize),
        Style::default().fg(Color::DarkGray),
    ));

    let mut lines = vec![header, separator];

    for raw_line in req.markdown.lines() {
        lines.push(Line::from(Span::raw(raw_line.to_owned())));
    }

    Text::from(lines)
}

/// Build a single-line error `Text` for use in the sentinel result after a renderer panic.
fn error_text(msg: &str) -> Text<'static> {
    Text::from(Line::from(Span::styled(
        msg.to_owned(),
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
    )))
}

// ---------------------------------------------------------
// Tests
// ---------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn req(revision: u64) -> RenderRequest {
        RenderRequest {
            revision,
            markdown: format!("# Revision {revision}"),
            col_width: 80,
        }
    }

    /// Spawn a worker with a short debounce and verify it renders the latest revision.
    #[test]
    fn worker_renders_latest_revision() {
        let (tx, rx, _handle) = spawn_worker(50);

        // Send 5 rapid requests.
        for i in 1u64..=5 {
            tx.send(req(i)).expect("send failed");
        }

        // Wait long enough for the worker to finish (debounce + render time).
        std::thread::sleep(Duration::from_millis(300));

        // Drain all results.
        let mut results: Vec<RenderResult> = Vec::new();
        while let Ok(r) = rx.try_recv() {
            results.push(r);
        }

        // At least 1 result must have arrived.
        assert!(!results.is_empty(), "expected at least one render result");

        // The final result must correspond to the latest revision (5).
        let last = results.last().unwrap();
        assert_eq!(
            last.revision, 5,
            "expected final result revision = 5, got {}",
            last.revision
        );
    }

    /// Stale results (revision < current) must not overwrite a newer cached preview.
    /// This is enforced in App::tick(), but we verify revision tagging is correct here.
    #[test]
    fn result_carries_correct_revision() {
        let (tx, rx, _handle) = spawn_worker(30);

        tx.try_send(req(42)).unwrap();
        std::thread::sleep(Duration::from_millis(200));

        let result = rx.try_recv().expect("expected a result");
        assert_eq!(result.revision, 42);
    }

    /// Verify the debounce actually coalesces rapid requests - if we send N requests quickly, we should receive far fewer than N results (ideally 1).
    #[test]
    fn debounce_coalesces_rapid_requests() {
        let debounce_ms = 80u64;
        let (tx, rx, _handle) = spawn_worker(debounce_ms);

        let n = 10u64;
        let t0 = Instant::now();

        for i in 1..=n {
            let _ = tx.try_send(req(i));

            // Space them out less than the debounce window so they all land in one burst.
            std::thread::sleep(Duration::from_millis(5));
        }

        // Wait for the debounce window to expire + some render time.
        let elapsed = t0.elapsed();
        let remaining = Duration::from_millis(debounce_ms * 3).saturating_sub(elapsed);
        std::thread::sleep(remaining);

        let mut count = 0usize;
        while rx.try_recv().is_ok() {
            count += 1;
        }

        // With 10 requests spaced 5ms apart and an 80ms debounce, we expect 1-2 renders, definitely <10.
        assert!(
            count < (n as usize),
            "expected debounce to coalesce requests; got {count} renders for {n} requests"
        );
    }
}
