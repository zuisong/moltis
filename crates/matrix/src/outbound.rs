use std::time::Duration;

use {
    async_trait::async_trait,
    matrix_sdk::ruma::{
        self, OwnedEventId, OwnedRoomId,
        events::{
            reaction::ReactionEventContent, relation::Annotation,
            room::message::RoomMessageEventContent,
        },
    },
    tracing::{debug, warn},
};

use {
    moltis_channels::{
        Error as ChannelError, Result as ChannelResult,
        plugin::{
            ChannelOutbound, ChannelStreamOutbound, ChannelThreadContext, InteractiveMessage,
            StreamEvent, StreamReceiver, ThreadMessage,
        },
    },
    moltis_common::types::ReplyPayload,
};

use crate::state::AccountStateMap;

/// Minimum chars before the first message is sent during streaming.
const STREAM_MIN_INITIAL_CHARS: usize = 30;

/// Throttle interval between edit-in-place updates during streaming.
const STREAM_EDIT_THROTTLE: Duration = Duration::from_millis(500);

/// Typing indicator refresh interval.
const TYPING_REFRESH_INTERVAL: Duration = Duration::from_secs(4);

pub struct MatrixOutbound {
    pub accounts: AccountStateMap,
}

impl MatrixOutbound {
    fn get_room(&self, account_id: &str, room_id_str: &str) -> ChannelResult<matrix_sdk::Room> {
        let accounts = self.accounts.read().unwrap_or_else(|e| e.into_inner());
        let state = accounts
            .get(account_id)
            .ok_or_else(|| ChannelError::unknown_account(account_id))?;
        let client = state.client.clone();
        drop(accounts);

        let room_id: OwnedRoomId = room_id_str
            .parse()
            .map_err(|e: ruma::IdParseError| ChannelError::invalid_input(e.to_string()))?;

        client
            .get_room(&room_id)
            .ok_or_else(|| ChannelError::unavailable(format!("room not found: {room_id_str}")))
    }
}

#[async_trait]
impl ChannelOutbound for MatrixOutbound {
    async fn send_text(
        &self,
        account_id: &str,
        to: &str,
        text: &str,
        _reply_to: Option<&str>,
    ) -> ChannelResult<()> {
        let room = self.get_room(account_id, to)?;
        let content = RoomMessageEventContent::text_markdown(text);
        room.send(content)
            .await
            .map_err(|e| ChannelError::external("matrix send_text", e))?;
        Ok(())
    }

    async fn send_media(
        &self,
        account_id: &str,
        to: &str,
        payload: &ReplyPayload,
        _reply_to: Option<&str>,
    ) -> ChannelResult<()> {
        let room = self.get_room(account_id, to)?;

        if !payload.text.is_empty() {
            let content = RoomMessageEventContent::text_markdown(&payload.text);
            room.send(content)
                .await
                .map_err(|e| ChannelError::external("matrix send_media text", e))?;
        }

        if let Some(media) = &payload.media {
            debug!(account_id, url = %media.url, "media attachment (URL-based upload not yet implemented)");
        }

        Ok(())
    }

    async fn send_typing(&self, account_id: &str, to: &str) -> ChannelResult<()> {
        let room = self.get_room(account_id, to)?;
        let _ = room.typing_notice(true).await;
        Ok(())
    }

    async fn send_html(
        &self,
        account_id: &str,
        to: &str,
        html: &str,
        _reply_to: Option<&str>,
    ) -> ChannelResult<()> {
        let room = self.get_room(account_id, to)?;
        let content = RoomMessageEventContent::text_html(html_to_plain(html), html);
        room.send(content)
            .await
            .map_err(|e| ChannelError::external("matrix send_html", e))?;
        Ok(())
    }

    async fn send_interactive(
        &self,
        account_id: &str,
        to: &str,
        message: &InteractiveMessage,
        reply_to: Option<&str>,
    ) -> ChannelResult<()> {
        // Matrix doesn't have native buttons — text fallback
        let mut text = message.text.clone();
        for row in &message.button_rows {
            text.push('\n');
            for btn in row {
                text.push_str(&format!("\n  [{}]", btn.label));
            }
        }
        self.send_text(account_id, to, &text, reply_to).await
    }

    async fn add_reaction(
        &self,
        account_id: &str,
        channel_id: &str,
        message_id: &str,
        emoji: &str,
    ) -> ChannelResult<()> {
        let room = self.get_room(account_id, channel_id)?;
        let event_id: OwnedEventId = message_id
            .parse()
            .map_err(|e: ruma::IdParseError| ChannelError::invalid_input(e.to_string()))?;

        let annotation = Annotation::new(event_id, emoji.to_string());
        let content = ReactionEventContent::new(annotation);
        room.send(content)
            .await
            .map_err(|e| ChannelError::external("matrix add_reaction", e))?;
        Ok(())
    }

    async fn remove_reaction(
        &self,
        account_id: &str,
        _channel_id: &str,
        message_id: &str,
        emoji: &str,
    ) -> ChannelResult<()> {
        warn!(
            account_id,
            message_id,
            emoji,
            "remove_reaction not yet implemented for Matrix, reaction will remain visible"
        );
        Ok(())
    }
}

#[async_trait]
impl ChannelStreamOutbound for MatrixOutbound {
    async fn send_stream(
        &self,
        account_id: &str,
        to: &str,
        _reply_to: Option<&str>,
        mut stream: StreamReceiver,
    ) -> ChannelResult<()> {
        let room = self.get_room(account_id, to)?;

        let mut buffer = String::new();
        let mut sent_event_id: Option<OwnedEventId> = None;
        let mut last_edit = tokio::time::Instant::now();

        let _ = room.typing_notice(true).await;
        let mut typing_refresh = tokio::time::interval(TYPING_REFRESH_INTERVAL);
        typing_refresh.tick().await;

        loop {
            tokio::select! {
                _ = typing_refresh.tick() => {
                    if sent_event_id.is_none() {
                        let _ = room.typing_notice(true).await;
                    }
                }
                event = stream.recv() => {
                    match event {
                        Some(StreamEvent::Delta(chunk)) => {
                            buffer.push_str(&chunk);

                            if sent_event_id.is_none() && buffer.len() >= STREAM_MIN_INITIAL_CHARS {
                                let content = RoomMessageEventContent::text_markdown(&buffer);
                                match room.send(content).await {
                                    Ok(response) => {
                                        sent_event_id = Some(response.event_id);
                                        last_edit = tokio::time::Instant::now();
                                        let _ = room.typing_notice(false).await;
                                    }
                                    Err(e) => {
                                        warn!("stream initial send failed: {e}");
                                        return Err(ChannelError::external("matrix stream", e));
                                    }
                                }
                            } else if sent_event_id.is_some()
                                && last_edit.elapsed() >= STREAM_EDIT_THROTTLE
                                && let Some(eid) = &sent_event_id
                            {
                                let edit = make_edit_content(eid, &buffer);
                                if let Err(e) = room.send(edit).await {
                                    warn!("stream edit failed: {e}");
                                }
                                last_edit = tokio::time::Instant::now();
                            }
                        }
                        Some(StreamEvent::Done) => {
                            if let Some(eid) = &sent_event_id {
                                let edit = make_edit_content(eid, &buffer);
                                let _ = room.send(edit).await;
                            } else if !buffer.is_empty() {
                                let content = RoomMessageEventContent::text_markdown(&buffer);
                                let _ = room.send(content).await;
                            }
                            let _ = room.typing_notice(false).await;
                            break;
                        }
                        Some(StreamEvent::Error(e)) => {
                            warn!("stream error: {e}");
                            if !buffer.is_empty() {
                                buffer.push_str("\n\n[stream error]");
                                if let Some(eid) = &sent_event_id {
                                    let edit = make_edit_content(eid, &buffer);
                                    let _ = room.send(edit).await;
                                } else {
                                    let content = RoomMessageEventContent::text_markdown(&buffer);
                                    let _ = room.send(content).await;
                                }
                            }
                            let _ = room.typing_notice(false).await;
                            break;
                        }
                        None => {
                            let _ = room.typing_notice(false).await;
                            break;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    async fn is_stream_enabled(&self, _account_id: &str) -> bool {
        true
    }
}

#[async_trait]
impl ChannelThreadContext for MatrixOutbound {
    async fn fetch_thread_messages(
        &self,
        account_id: &str,
        channel_id: &str,
        thread_id: &str,
        limit: usize,
    ) -> ChannelResult<Vec<ThreadMessage>> {
        debug!(
            account_id,
            channel_id, thread_id, limit, "fetch_thread_messages not yet implemented"
        );
        Ok(Vec::new())
    }
}

/// Create an m.replace edit event content.
fn make_edit_content(original_event_id: &OwnedEventId, new_body: &str) -> RoomMessageEventContent {
    use matrix_sdk::ruma::events::room::message::ReplacementMetadata;
    let new_content = RoomMessageEventContent::text_markdown(new_body);
    let metadata = ReplacementMetadata::new(original_event_id.clone(), None);
    new_content.make_replacement(metadata)
}

/// Simple HTML to plain text conversion.
fn html_to_plain(html: &str) -> String {
    let mut plain = String::with_capacity(html.len());
    let mut remaining = html;

    while let Some(ch) = remaining.chars().next() {
        if ch == '<' {
            if let Some((tag_name, consumed_len)) = consume_html_tag(remaining) {
                if is_plain_text_line_break_tag(&tag_name) && !plain.ends_with('\n') {
                    plain.push('\n');
                }
                remaining = &remaining[consumed_len..];
                continue;
            }
        } else if ch == '&'
            && let Some((decoded, consumed_len)) = consume_html_entity(remaining)
        {
            plain.push_str(&decoded);
            remaining = &remaining[consumed_len..];
            continue;
        }

        plain.push(ch);
        remaining = &remaining[ch.len_utf8()..];
    }

    plain.trim_matches('\n').to_string()
}

fn consume_html_tag(input: &str) -> Option<(String, usize)> {
    let bytes = input.as_bytes();
    if bytes.first().copied()? != b'<' {
        return None;
    }

    let mut index = 1usize;
    if bytes.get(index).copied() == Some(b'/') {
        index += 1;
    }

    let name_start = index;
    let first = bytes.get(index).copied()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }

    while let Some(next) = bytes.get(index).copied() {
        if next.is_ascii_alphanumeric() || next == b'-' {
            index += 1;
            continue;
        }
        break;
    }

    let tag_name = input[name_start..index].to_ascii_lowercase();

    let mut quote = None;
    while let Some(next) = bytes.get(index).copied() {
        match quote {
            Some(delimiter) if next == delimiter => quote = None,
            Some(_) => {},
            None if next == b'\'' || next == b'"' => quote = Some(next),
            None if next == b'>' => return Some((tag_name, index + 1)),
            None => {},
        }
        index += 1;
    }

    None
}

fn consume_html_entity(input: &str) -> Option<(String, usize)> {
    if !input.starts_with('&') {
        return None;
    }

    let mut entity = String::new();
    for (index, ch) in input.char_indices() {
        entity.push(ch);
        let consumed_len = index + ch.len_utf8();
        if ch == ';' {
            return decode_html_entity(&entity).map(|decoded| (decoded, consumed_len));
        }
        if entity.len() > 12 {
            return None;
        }
    }

    None
}

fn decode_html_entity(entity: &str) -> Option<String> {
    match entity {
        "&amp;" => Some("&".to_string()),
        "&lt;" => Some("<".to_string()),
        "&gt;" => Some(">".to_string()),
        "&quot;" => Some("\"".to_string()),
        "&apos;" | "&#39;" => Some("'".to_string()),
        "&nbsp;" | "&#160;" => Some(" ".to_string()),
        _ => decode_numeric_html_entity(entity),
    }
}

fn decode_numeric_html_entity(entity: &str) -> Option<String> {
    let value = entity
        .strip_prefix("&#x")
        .or_else(|| entity.strip_prefix("&#X"))
        .and_then(|hex| hex.strip_suffix(';'))
        .and_then(|hex| u32::from_str_radix(hex, 16).ok())
        .or_else(|| {
            entity
                .strip_prefix("&#")
                .and_then(|decimal| decimal.strip_suffix(';'))
                .and_then(|decimal| decimal.parse::<u32>().ok())
        })?;

    char::from_u32(value).map(|ch| ch.to_string())
}

fn is_plain_text_line_break_tag(tag_name: &str) -> bool {
    matches!(
        tag_name,
        "blockquote" | "br" | "div" | "li" | "ol" | "p" | "pre" | "tr" | "ul"
    )
}

#[cfg(test)]
mod tests {
    use super::html_to_plain;

    #[test]
    fn html_to_plain_strips_unknown_tags_and_decodes_entities() {
        let plain = html_to_plain(
            "<div>Hello <span>world</span></div><script>alert(&quot;ok&quot;)</script>",
        );

        assert_eq!(plain, "Hello world\nalert(\"ok\")");
    }

    #[test]
    fn html_to_plain_preserves_non_tag_angle_bracket_text() {
        let plain = html_to_plain("<code>if a < b && c > d</code>");

        assert_eq!(plain, "if a < b && c > d");
    }
}
