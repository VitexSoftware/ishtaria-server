ALTER TABLE players
    ADD COLUMN movement_sequence BIGINT NOT NULL DEFAULT 0 CHECK (movement_sequence >= 0),
    ADD COLUMN last_moved_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp();