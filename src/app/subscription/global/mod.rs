mod input;
mod mouse;
mod window;

use std::sync::atomic::AtomicBool;

use iced::{
    futures::{SinkExt, Stream, StreamExt, channel::mpsc::channel},
    stream,
};
use keyboard_types::Modifiers;

/// Tracks whether the Win key is currently held. The rdev keyboard hook
/// swallows Win-key events on modern Windows, which prevents the OS from
/// updating the GetAsyncKeyState bit — so we have to keep our own state and
/// expose it for other low-level hooks (mouse-wheel scrolling, etc.) to read.
pub static WIN_DOWN: AtomicBool = AtomicBool::new(false);

use crate::app::{Message, subscription::STREAM_CHANNEL_BUFFER_SIZE};

#[derive(Debug, Clone)]
pub enum GlobalMessage {
    Key(Modifiers, rdev::Key),
    Window,
    /// Win+wheel scroll, in pre-signed pixels (positive = scroll tile strip
    /// rightward, negative = leftward).
    HorizontalScroll {
        delta_px: f32,
    },
}

pub fn subscription() -> impl Stream<Item = Message> {
    stream::channel(STREAM_CHANNEL_BUFFER_SIZE, async |mut output| {
        let (intermediate_message_tx, mut intermediate_message_rx) = channel(100);

        let global_input_tx = intermediate_message_tx.clone();
        let window_event_tx = intermediate_message_tx;

        input::launch(global_input_tx.clone());
        mouse::launch(global_input_tx);
        window::launch(window_event_tx);

        while let Some(event) = intermediate_message_rx.next().await {
            output.send(Message::Global(event)).await.unwrap();
        }
    })
}
