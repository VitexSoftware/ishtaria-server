-- Friends and short-lived events. Requests wait for an answer; accepted friendships live
-- in player_friendships (migration 0006). Events (a friend reached a level) are delivered
-- once and then forgotten: they are not a history and are removed after a short time.

ALTER TABLE players ADD COLUMN last_seen_at TIMESTAMPTZ;

CREATE TABLE friend_requests (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    from_id BIGINT NOT NULL REFERENCES players(id) ON DELETE CASCADE,
    to_id BIGINT NOT NULL REFERENCES players(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (from_id, to_id),
    CHECK (from_id <> to_id)
);
CREATE INDEX friend_requests_to ON friend_requests (to_id);

CREATE TABLE player_events (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    recipient_id BIGINT NOT NULL REFERENCES players(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('level_up')),
    subject TEXT NOT NULL CHECK (char_length(subject) <= 64),
    level INTEGER,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX player_events_recipient ON player_events (recipient_id, id);
CREATE INDEX player_events_created ON player_events (created_at);
