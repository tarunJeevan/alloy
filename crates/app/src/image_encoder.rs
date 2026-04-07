//! Protocol-encoding thread for terminal image rendering.
//!
//! Architecture:
//!
//! - Perform `Picker::new_resize_protocol` encoding on a thread separate from the UI render thread to ensure there are no stalls in UI responsiveness while rendering images.
//! - UI's `tick()` becomes non-blocking and accepts a ready `StatefulImage` if one is available and continues with the placeholder of it isn't available.
//!
//! Thread safety:
//!
//! - `StatefulProtocol` implements `Send`, making this transferable across threads.
//! - `Picker` is also `Send` so it can be moved into the encoder thread at startup and never touched by the UI thread after that.
//!
//! Back-pressure:
//!
//! - The request channel is bounded (capacity of 16).
//! - The UI thread uses `try_send` and silently drops if the channel is full - the image will be submitted again on the next `RenderResult` that contains it.
//! - This prevents unbounded memory growth if the encoder thread falls behind (e.g. multiple large images).

use std::sync::mpsc::{Receiver, SyncSender, sync_channel};

use image::DynamicImage;
use ratatui_image::{picker::Picker, protocol::StatefulProtocol};

// ----------------------------------------------------------------------
// Channel types
// ----------------------------------------------------------------------

/// Sent from UI `tick` to encoder thread.
pub struct EncodeRequest {
    /// The raw URL/path string - used as the cache key in `App::protocol_cache`.
    pub key: String,

    /// The decoded image to encode.
    pub image: DynamicImage,
}

/// Sent from the encoder thread back to the UI thread.
pub struct EncodeResult {
    /// Same key as the request, for insertion into `App::protocol_cache`.
    pub key: String,

    /// The protocol-encoded image, ready for `render_stateful_widget`.
    pub protocol: StatefulProtocol,
}

// ----------------------------------------------------------------------
// Spawn
// ----------------------------------------------------------------------

/// Spawn the encoder thread and return `(request_sender, result_receiver)`.
///
/// `picker` is moved into the thread and used exclusively there - the UI thread must not retain a reference to it after this call.
pub fn spawn_encoder(picker: Picker) -> (SyncSender<EncodeRequest>, Receiver<EncodeResult>) {
    let (req_tx, req_rx) = sync_channel::<EncodeRequest>(16);
    let (res_tx, res_rx) = std::sync::mpsc::channel::<EncodeResult>();

    std::thread::Builder::new()
        .name("image-encoder".into())
        .spawn(move || encoder_loop(picker, req_rx, res_tx))
        .expect("failed to spawn image-encoder thread");

    (req_tx, res_rx)
}

// ----------------------------------------------------------------------
// Encoder loop
// ----------------------------------------------------------------------

fn encoder_loop(
    picker: Picker,
    req_rx: Receiver<EncodeRequest>,
    res_tx: std::sync::mpsc::Sender<EncodeResult>,
) {
    // Process image render requests one at a time. Each call to `new_resize_protocol` may take 10-100ms.
    for req in req_rx {
        tracing::debug!(key = %req.key, "image-encoder: encoding image");

        let protocol = picker.new_resize_protocol(req.image);

        if res_tx
            .send(EncodeResult {
                key: req.key,
                protocol,
            })
            .is_err()
        {
            // UI thread dropped its receiver (i.e. app is shutting down).
            tracing::debug!("image-encoder: result channel closed. Exiting...");
            return;
        }
    }

    // req_rx iterator ends when the sender is dropped (App dropped).
    tracing::debug!("image-encoder: request channel closed. Exiting...")
}
