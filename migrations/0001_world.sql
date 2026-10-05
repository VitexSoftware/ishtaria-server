CREATE TABLE worlds (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    server_name TEXT NOT NULL UNIQUE,
    ruleset TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE heightmaps (
    world_id BIGINT PRIMARY KEY REFERENCES worlds(id) ON DELETE CASCADE,
    seed TEXT NOT NULL CHECK (seed ~ '^[0-9]+$'),
    face_size INTEGER NOT NULL CHECK (face_size > 0 AND face_size <= 1024),
    sha256 TEXT NOT NULL CHECK (length(sha256) = 64),
    pgm BYTEA NOT NULL,
    pixels BYTEA NOT NULL,
    imported_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CHECK (octet_length(pixels) = 6 * face_size * face_size)
);