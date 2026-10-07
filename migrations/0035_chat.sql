-- Text chat (ADR 0007): the server relays a message and forgets it. Messages travel as
-- short-lived events: a spoken line lives for as long as an event does, a whisper to an
-- offline friend waits in the mailbox for seven days (at most 100 per recipient, enforced
-- by the application). No language or history is stored.

ALTER TABLE player_events
    DROP CONSTRAINT player_events_kind_check,
    ADD CONSTRAINT player_events_kind_check CHECK (kind IN ('level_up', 'say', 'whisper')),
    ADD COLUMN body TEXT CHECK (body IS NULL OR char_length(body) BETWEEN 1 AND 500),
    ADD COLUMN expires_at TIMESTAMPTZ,
    ADD CONSTRAINT player_events_message_body CHECK ((kind IN ('say', 'whisper')) = (body IS NOT NULL));

CREATE INDEX player_events_expires ON player_events (expires_at) WHERE expires_at IS NOT NULL;

-- Rate limiting of senders: one message per second is enough for talking.
ALTER TABLE players ADD COLUMN last_chat_at TIMESTAMPTZ;
