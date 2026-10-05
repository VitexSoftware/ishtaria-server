-- An optional flag a player chooses to show next to their name. It is only a picture
-- to display: it is not a language preference and nothing is translated by it. The
-- client's own language stays in the client; the flag is sent only when the player opts in.
ALTER TABLE players ADD COLUMN flag TEXT CHECK (flag IS NULL OR flag ~ '^[A-Z]{2}$');
