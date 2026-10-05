-- The hall of fame ranks characters by the wealth they reached.
CREATE INDEX players_wealth ON players (world_id, lifetime_gold DESC, created_at) WHERE banned_at IS NULL;
