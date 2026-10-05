-- Operator administration: player bans, saved world maps, portals to linked
-- worlds and a scheduled server shutdown. Managed by ishtaria-admin.
ALTER TABLE players
    ADD COLUMN banned_at TIMESTAMPTZ,
    ADD COLUMN ban_reason TEXT CHECK (char_length(ban_reason) <= 500);

CREATE TABLE world_maps (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    world_id BIGINT NOT NULL REFERENCES worlds(id),
    name TEXT NOT NULL CHECK (char_length(name) BETWEEN 1 AND 64),
    seed TEXT NOT NULL,
    face_size INTEGER NOT NULL CHECK (face_size BETWEEN 1 AND 1024),
    sha256 TEXT NOT NULL,
    pgm BYTEA NOT NULL,
    pixels BYTEA NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (world_id, name)
);

CREATE TABLE portals (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    world_id BIGINT NOT NULL REFERENCES worlds(id),
    name TEXT NOT NULL CHECK (name ~ '^[a-z0-9-]{1,64}$'),
    peer TEXT CHECK (peer ~ '^[a-z0-9.-]{1,253}$'),
    state TEXT NOT NULL DEFAULT 'building'
        CHECK (state IN ('building', 'open', 'closed', 'disabled', 'banned')),
    face INTEGER NOT NULL CHECK (face BETWEEN 0 AND 5),
    x INTEGER NOT NULL CHECK (x >= 0),
    y INTEGER NOT NULL CHECK (y >= 0),
    capacity_per_hour INTEGER CHECK (capacity_per_hour >= 1),
    max_cargo_items INTEGER CHECK (max_cargo_items >= 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (world_id, name)
);

CREATE TABLE server_shutdown (
    world_id BIGINT PRIMARY KEY REFERENCES worlds(id),
    requested_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    shutdown_at TIMESTAMPTZ NOT NULL,
    message TEXT CHECK (char_length(message) <= 200),
    requested_by TEXT NOT NULL DEFAULT current_user
);
