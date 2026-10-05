-- Equipment: the item a character holds in hand. Tools and weapons are used only
-- when equipped; new characters start with the axe in hand.

CREATE TABLE player_equipment (
    player_id BIGINT PRIMARY KEY REFERENCES players(id) ON DELETE CASCADE,
    hand TEXT NOT NULL REFERENCES item_types(id)
);

INSERT INTO player_equipment (player_id, hand)
    SELECT player_id, 'axe' FROM player_inventory
    JOIN players ON players.id = player_inventory.player_id
    WHERE item_id = 'axe' AND died_at IS NULL
    ON CONFLICT (player_id) DO NOTHING;

CREATE OR REPLACE FUNCTION starter_inventory() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO player_inventory (player_id, item_id, quantity) VALUES
        (NEW.id, 'apple', 3), (NEW.id, 'bread', 3), (NEW.id, 'cheese', 2), (NEW.id, 'carrot', 2),
        (NEW.id, 'gold', 100),
        (NEW.id, 'axe', 1), (NEW.id, 'pickaxe', 1), (NEW.id, 'sword', 1);
    INSERT INTO player_equipment (player_id, hand) VALUES (NEW.id, 'axe');
    RETURN NEW;
END;
$$;
