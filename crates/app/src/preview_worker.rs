//! Background preview render worker.
//!
//! Runs on a dedicated `std::thread`. The UI thread sends `RenderRequest`s via a bounded `SyncSender`. The worker debounces, renders, and sends `RenderResult`s back.
//!
//! Dual-output design:
//!
//! - The worker always produces both `Text<'static>` via `PulldownEngine` and `String` via `ComrakEngine` in a single render cycle. The UI chooses which to display based on `PreviewMode`. This avoids a second render pass on mode toggle and keeps the worker stateless with respect to the preview mode.
//! - HTML generation via comrak is fast (typically <1 ms for moderate documents) and negligible compared with the debounce window, so the cost is unconditionally paid.
//!
//! Debounce algorithm (recv_timeout):
//!
//! 1. Block on `recv()` waiting for the first request.
//! 2. Enter the drain loop: call `recv_timeout(debounce_ms)`.
//!   - If a newer request arrives before the timeout, replace current request loop.
//!   - If the timeout fires (no new request within the debounce window), exit drain loop and render the most recent request.
//! 3. Send the `RenderResult` back via `result_sender`.
//! 4. Go to step 1.
//!
//! Panic safety:
//!
//! - The worker body is wrapped in `std::panic::catch_unwind`. On panic, a sentinel `RenderResult` is sent to the UI so the preview pane shows an error message rather than silently going stale.
//!
//! Channel sizing:
//!
//! - The request channel is BOUNDED (`sync_channel(4)`). The UI uses `try_send` and silently drops if the channel is full. The next keystroke will trigger a fresh request anyway. This ensures the worker is never starved of CPU by a backlog of stale render jobs.

use std::sync::{
    Arc,
    mpsc::{Receiver, SyncSender, sync_channel},
};
use std::time::Duration;

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
};

use markdown::{
    ComrakEngine, ComrakExtensions, MarkdownEngine, PulldownEngine,
    engines::pulldown::EngineExtensions,
};

// ---------------------------------------------------------
// WorkerExtensions
// ---------------------------------------------------------

/// All extension flags the worker needs to configure both engines.
///
/// Constructed in `App::new` from `Config::markdown.extensions` by copying the relevant booleans. Kept as a separate type from `EngineExtensions` and `ComrakExtensions` so neither `app` nor `markdown` crates need to know about each other's internal types at this boundary.
#[derive(Debug, Clone, Default)]
pub struct WorkerExtensions {
    pub gfm: bool,
    pub wiki_links: bool,
    pub footnotes: bool,
    pub frontmatter: bool,
    pub math: bool,
}

impl WorkerExtensions {
    fn to_engine_extensions(&self) -> EngineExtensions {
        EngineExtensions {
            gfm: self.gfm,
            footnotes: self.footnotes,
        }
    }

    fn to_comrak_extensions(&self) -> ComrakExtensions {
        ComrakExtensions {
            gfm: self.gfm,
            wiki_links: self.wiki_links,
            footnotes: self.footnotes,
            frontmatter: self.frontmatter,
            math: self.math,
        }
    }
}

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
    pub col_width: u16,
}

/// A render result sent from the worker back to the UI thread.
///
/// Both `rendered` and `html` are always populated regardless of the current `PreviewMode`. The UI selects which to display.
#[derive(Debug)]
pub struct RenderResult {
    /// The revision this result corresponds to.
    pub revision: u64,

    /// Terminal-rendered markdown (via `PulldownEngine`).
    pub rendered: Text<'static>,

    /// Raw HTML stirng (via `ComrakEngine`).
    pub html: String,
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
    extensions: WorkerExtensions,
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

    // Construct the terminal engine (PulldownEngine) for fast text rendering.
    let terminal_engine: Arc<dyn MarkdownEngine> =
        Arc::new(PulldownEngine::new(extensions.to_engine_extensions()));

    // Construct the HTML engine (ComrakEngine) for HTML output.
    let html_engine: Arc<dyn MarkdownEngine> =
        Arc::new(ComrakEngine::new(extensions.to_comrak_extensions()));

    let handle = std::thread::Builder::new()
        .name("preview-worker".into())
        .spawn(move || {
            worker_loop(
                req_rx,
                res_tx,
                Duration::from_millis(debounce_ms),
                terminal_engine,
                html_engine,
            );
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
    terminal_engine: Arc<dyn MarkdownEngine>,
    html_engine: Arc<dyn MarkdownEngine>,
) {
    loop {
        // Block until the first request arrives (or the channel is closed, in which case exit).
        let first = match req_rx.recv() {
            Ok(r) => r,
            Err(_) => {
                tracing::debug!("preview worker: request channel closed. Exiting...");
                return;
            }
        };

        // Drain loop - keep replacing `current` with newer requests until the debounce window expires with no new arrivals.
        let current = drain_to_latest(first, &req_rx, debounce);

        let revision = current.revision;
        let col_width = current.col_width;
        let markdown = current.markdown.clone();

        // A newtype wrapper to unconditionally implement UnwindSafe on MarkdownEngine to satisfy compiler requirements.
        use std::panic::AssertUnwindSafe;

        // Render terminal inside catch_unwind so a panic in the renderer doesn't kill the worker.
        // SAFETY: PulldownEngine holds only plain config flags (no interior mutability). AssertUnwindSafe is correct here. Revisit if a stateful engine is ever added.
        let rendered = match std::panic::catch_unwind(AssertUnwindSafe(|| {
            terminal_engine.render_terminal(&markdown, col_width)
        })) {
            Ok(t) => t,
            Err(_) => {
                tracing::error!(
                    "preview worker: terminal renderer panicked for revision {revision}"
                );
                error_text("Preview renderer panicked. Please check logs for details.")
            }
        };

        let html =
            match std::panic::catch_unwind(AssertUnwindSafe(|| html_engine.render_html(&markdown)))
            {
                Ok(h) => h,
                Err(_) => {
                    tracing::error!(
                        "preview worker: HTML renderer panicked for revision {revision}"
                    );
                    "<!-- HTML renderer panicked. Please check logs for more detail -->".to_owned()
                }
            };

        // Send the result. If the UI has dropped its receiver (app is shutting down), exit.
        if res_tx
            .send(RenderResult {
                revision,
                rendered,
                html,
            })
            .is_err()
        {
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
// Error sentinel
// ---------------------------------------------------------

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
    use std::time::Duration;

    fn default_extensions() -> WorkerExtensions {
        WorkerExtensions {
            gfm: true,
            ..Default::default()
        }
    }

    fn req(revision: u64) -> RenderRequest {
        RenderRequest {
            revision,
            markdown: format!("# Revision {revision}\n\nSome content."),
            col_width: 80,
        }
    }

    /// Spawn a worker with a short debounce and verify it renders the latest revision.
    #[test]
    fn worker_renders_latest_revision() {
        let (tx, rx, _handle) = spawn_worker(50, default_extensions());

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
        let (tx, rx, _handle) = spawn_worker(30, default_extensions());

        tx.try_send(req(42)).unwrap();
        std::thread::sleep(Duration::from_millis(200));

        let result = rx.try_recv().expect("expected a result");
        assert_eq!(result.revision, 42);
    }

    #[test]
    fn rendered_text_contains_heading_content() {
        let (tx, rx, _handle) = spawn_worker(30, default_extensions());

        tx.try_send(RenderRequest {
            revision: 1,
            markdown: "# Hello Alloy\n\nSome paragraph.\n".to_owned(),
            col_width: 80,
        })
        .unwrap();

        std::thread::sleep(Duration::from_millis(200));

        let result = rx.try_recv().expect("expected a result");

        let plain: String = result
            .rendered
            .lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();

        assert!(
            plain.contains("Hello Alloy"),
            "rendered output should contain heading text: {plain:?}"
        );
    }

    #[test]
    fn html_field_contains_h1_tag() {
        let (tx, rx, _handle) = spawn_worker(30, default_extensions());

        tx.try_send(RenderRequest {
            revision: 1,
            markdown: "# My Heading\n\nParagraph text.\n".to_owned(),
            col_width: 80,
        })
        .unwrap();

        std::thread::sleep(Duration::from_millis(200));

        let result = rx.try_recv().expect("expected a result");
        assert!(
            result.html.contains("<h1>") || result.html.contains("<h1 "),
            "html field should contain h1 tag: {:?}",
            result.html
        );
    }

    /// Verify the debounce actually coalesces rapid requests - if we send N requests quickly, we should receive far fewer than N results (ideally 1).
    #[test]
    fn debounce_coalesces_rapid_requests() {
        let debounce_ms = 80u64;
        let (tx, rx, _handle) = spawn_worker(debounce_ms, default_extensions());

        let n = 10u64;
        for i in 1..=n {
            let _ = tx.try_send(req(i));
            // Space them out less than the debounce window so they all land in one burst.
            std::thread::sleep(Duration::from_millis(5));
        }

        // Wait for the debounce window to expire + some render time.
        std::thread::sleep(Duration::from_millis(debounce_ms * 3));

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
