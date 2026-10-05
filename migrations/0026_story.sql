-- Story datadisks: the disks a world was generated with, where their places stand,
-- and each player's progress through dialogues and quests.

-- Disks selected for a saved map; copied to world_datadisks when the map is loaded.
ALTER TABLE world_maps ADD COLUMN datadisks JSONB NOT NULL DEFAULT '[]'
    CHECK (jsonb_typeof(datadisks) = 'array');

CREATE TABLE world_datadisks (
    world_id BIGINT NOT NULL REFERENCES worlds(id),
    disk_id TEXT NOT NULL CHECK (disk_id ~ '^[a-z][a-z0-9_]{1,31}$'),
    version TEXT NOT NULL CHECK (version ~ '^[0-9]+\.[0-9]+\.[0-9]+$'),
    -- Pinned by the server the first time it loads the disk after generation.
    sha256 TEXT CHECK (sha256 ~ '^[0-9a-f]{64}$'),
    position INTEGER NOT NULL CHECK (position >= 0),
    applied_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (world_id, disk_id)
);

-- Persisted sites of datadisk places. They are derived once from the seed and never move.
CREATE TABLE story_anchors (
    world_id BIGINT NOT NULL REFERENCES worlds(id),
    anchor_id TEXT NOT NULL CHECK (char_length(anchor_id) BETWEEN 3 AND 100),
    heightmap_sha256 TEXT NOT NULL CHECK (heightmap_sha256 ~ '^[0-9a-f]{64}$'),
    direction_x DOUBLE PRECISION NOT NULL,
    direction_y DOUBLE PRECISION NOT NULL,
    direction_z DOUBLE PRECISION NOT NULL,
    height_m DOUBLE PRECISION NOT NULL,
    placed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (world_id, anchor_id)
);

CREATE TABLE player_story_flags (
    player_id BIGINT NOT NULL REFERENCES players(id) ON DELETE CASCADE,
    flag TEXT NOT NULL CHECK (char_length(flag) BETWEEN 3 AND 100),
    PRIMARY KEY (player_id, flag)
);

CREATE TABLE player_quests (
    player_id BIGINT NOT NULL REFERENCES players(id) ON DELETE CASCADE,
    quest_id TEXT NOT NULL CHECK (char_length(quest_id) BETWEEN 3 AND 100),
    stage TEXT NOT NULL CHECK (char_length(stage) BETWEEN 1 AND 48),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (player_id, quest_id)
);

-- A `once` effect key is recorded when its rewards are granted, so they are granted once.
CREATE TABLE player_story_once (
    player_id BIGINT NOT NULL REFERENCES players(id) ON DELETE CASCADE,
    once_key TEXT NOT NULL CHECK (char_length(once_key) BETWEEN 3 AND 100),
    PRIMARY KEY (player_id, once_key)
);

-- The open conversation of a player. `seq` makes every answer replay-safe.
CREATE TABLE dialogue_sessions (
    player_id BIGINT PRIMARY KEY REFERENCES players(id) ON DELETE CASCADE,
    npc_id TEXT NOT NULL,
    node_id TEXT NOT NULL,
    seq BIGINT NOT NULL DEFAULT 0 CHECK (seq >= 0),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
