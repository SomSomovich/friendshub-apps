use std::sync::Arc;

use friendshub_core::EventHandler;
use friendshub_core::events::Event;

/// Forwards typed core event callbacks as owned `Event` values.
///
/// The core calls these methods from its own event worker, so the callback
/// must hand data to the Slint event loop instead of touching a component
/// directly.
pub struct CoreEventHandler {
    callback: Arc<dyn Fn(Event) + Send + Sync + 'static>,
}

impl CoreEventHandler {
    pub fn new(callback: impl Fn(Event) + Send + Sync + 'static) -> Self {
        Self {
            callback: Arc::new(callback),
        }
    }

    fn forward(&self, event: &Event) {
        (self.callback)(event.clone());
    }
}

macro_rules! forward_event_methods {
    ($($method:ident),+ $(,)?) => {
        impl EventHandler for CoreEventHandler {
            $(
                fn $method(&self, event: &Event) {
                    self.forward(event);
                }
            )+
        }
    };
}

forward_event_methods!(
    on_ws_connected,
    on_ws_disconnected,
    on_ws_stopped,
    on_envelope_received,
    on_message_received,
    on_poll_received,
    on_message_edited,
    on_message_deleted,
    on_message_reaction,
    on_attachment_key_received,
    on_call_signal_received,
    on_history_chunk_received,
    on_history_complete_received,
    on_sender_key_received,
    on_receipt,
    on_server_error,
    on_fatal_error,
    on_presence_updated,
    on_bot_message,
    on_channel_post,
    on_typing,
    on_read_receipt,
    on_conversation_updated,
    on_history_request,
    on_nudge,
    on_nudge_result,
    on_conversation_expired,
    on_scheduled_created,
    on_scheduled_cancelled,
    on_folders_updated,
    on_bot_message_edited,
    on_bot_message_deleted,
    on_bot_callback_answered,
    on_poll_updated,
    on_poll_deleted,
    on_call_signal,
    on_identity_changed,
    on_webrtc_ice_candidate,
    on_webrtc_signaling_state_changed,
    on_webrtc_ice_connection_state_changed,
    on_webrtc_ice_gathering_state_changed,
    on_webrtc_connection_state_changed,
    on_webrtc_data_channel_opened,
    on_webrtc_data_channel_closed,
    on_webrtc_data_channel_message,
    on_webrtc_track_received,
    on_webrtc_closed,
    on_unhandled,
);
