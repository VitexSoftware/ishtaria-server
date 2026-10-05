CREATE TABLE players (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    world_id BIGINT NOT NULL REFERENCES worlds(id) ON DELETE CASCADE,
    username TEXT NOT NULL CHECK (username ~ '^[a-zA-Z0-9_-]{3,32}$'),
    password_hash TEXT NOT NULL,
    gold BIGINT NOT NULL DEFAULT 100 CHECK (gold >= 0),
    health INTEGER NOT NULL DEFAULT 100 CHECK (health BETWEEN 0 AND 100),
    stamina INTEGER NOT NULL DEFAULT 100 CHECK (stamina BETWEEN 0 AND 100),
    food INTEGER NOT NULL DEFAULT 100 CHECK (food BETWEEN 0 AND 100),
    water INTEGER NOT NULL DEFAULT 100 CHECK (water BETWEEN 0 AND 100),
    level INTEGER NOT NULL DEFAULT 1 CHECK (level >= 1),
    experience BIGINT NOT NULL DEFAULT 0 CHECK (experience >= 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX players_world_username ON players (world_id, lower(username));

CREATE TABLE player_sessions (
    token_hash BYTEA PRIMARY KEY CHECK (octet_length(token_hash) = 32),
    player_id BIGINT NOT NULL REFERENCES players(id) ON DELETE CASCADE,
    expires_at TIMESTAMPTZ NOT NULL DEFAULT (now() + interval '1 day')
);

CREATE INDEX player_sessions_player ON player_sessions (player_id);