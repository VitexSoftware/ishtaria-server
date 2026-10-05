CREATE TABLE player_friendships (
    player_id BIGINT NOT NULL REFERENCES players(id),
    friend_id BIGINT NOT NULL REFERENCES players(id),
    PRIMARY KEY (player_id, friend_id),
    CHECK (player_id < friend_id)
);
CREATE INDEX player_friendships_friend_idx ON player_friendships(friend_id);

ALTER TABLE graves
    ADD COLUMN lived_days BIGINT NOT NULL DEFAULT 0 CHECK (lived_days >= 0),
    ADD COLUMN friends_count BIGINT NOT NULL DEFAULT 0 CHECK (friends_count >= 0);

UPDATE graves SET lived_days = greatest(0, floor(extract(epoch FROM (graves.died_at - players.created_at)) / 86400))::bigint
FROM players WHERE players.id = graves.player_id;