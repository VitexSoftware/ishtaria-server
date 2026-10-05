DROP INDEX players_world_username;
CREATE UNIQUE INDEX players_world_username
    ON players (world_id, lower(username)) WHERE died_at IS NULL;

ALTER TABLE graves ADD COLUMN born_at TIMESTAMPTZ;
UPDATE graves SET born_at = players.created_at
    FROM players WHERE players.id = graves.player_id;
ALTER TABLE graves ALTER COLUMN born_at SET NOT NULL;

CREATE TRIGGER permanent_grave_birth
BEFORE UPDATE OF born_at ON graves
FOR EACH ROW EXECUTE FUNCTION preserve_memorial();