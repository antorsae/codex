//! Retain an unfinished assistant message when a quota failure interrupts its stream.

use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;

#[derive(Default)]
pub(crate) struct PartialAssistantMessage(Option<ResponseItem>);

impl PartialAssistantMessage {
    pub(crate) fn start(&mut self, item: &ResponseItem) {
        self.0 = match item {
            ResponseItem::Message { role, .. } if role == "assistant" => Some(item.clone()),
            _ => None,
        };
    }

    pub(crate) fn append(&mut self, delta: &str) {
        if let Some(ResponseItem::Message { content, .. }) = &mut self.0 {
            if let Some(ContentItem::OutputText { text }) = content.last_mut() {
                text.push_str(delta);
            } else {
                content.push(ContentItem::OutputText {
                    text: delta.to_owned(),
                });
            }
        }
    }

    pub(crate) fn complete(&mut self) {
        self.0 = None;
    }

    pub(crate) fn take(&mut self) -> Option<ResponseItem> {
        self.0.take().filter(|item| matches!(item, ResponseItem::Message { content, .. }
            if content.iter().any(|item| matches!(item, ContentItem::OutputText { text } if !text.is_empty()))))
    }
}
