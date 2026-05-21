//! iced subscription that registers the API command channel and launches
//! the HTTP server thread. Yields `Message::Api(...)` events as the server
//! receives requests.

use iced::{
    futures::{SinkExt, Stream, StreamExt, channel::mpsc::channel},
    stream,
};

use crate::{api, app::Message, app::subscription::STREAM_CHANNEL_BUFFER_SIZE};

pub fn subscription() -> impl Stream<Item = Message> {
    stream::channel(STREAM_CHANNEL_BUFFER_SIZE, async |mut output| {
        let (tx, mut rx) = channel(STREAM_CHANNEL_BUFFER_SIZE);
        api::set_command_sender(tx);
        api::launch();

        while let Some(message) = rx.next().await {
            if let Err(e) = output.send(message).await {
                log::warn!("API subscription send failed: {e}");
                break;
            }
        }
    })
}
