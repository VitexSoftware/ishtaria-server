-- Gold coins become ordinary inventory items instead of a balance column.
-- Coins stack to 10000 per slot and, unlike other items, may occupy several
-- slots, so the number a player can carry (and take through a portal) is not
-- capped by the stack size. Existing balances are preserved exactly.

ALTER TABLE item_types ADD COLUMN multi_slot BOOLEAN NOT NULL DEFAULT false;
INSERT INTO item_types (id, name, calories, capacity_bonus, stack_limit, multi_slot)
    VALUES ('gold', 'Gold coin', 0, 0, 10000, true);

INSERT INTO player_inventory (player_id, item_id, quantity)
    SELECT id, 'gold', gold FROM players WHERE gold > 0;
INSERT INTO grave_inventory (grave_id, item_id, quantity)
    SELECT id, 'gold', gold FROM graves WHERE gold > 0;

DO $$
BEGIN
    IF coalesce((SELECT sum(gold) FROM players), 0)::numeric
        <> coalesce((SELECT sum(quantity) FROM player_inventory WHERE item_id = 'gold'), 0)::numeric
        OR coalesce((SELECT sum(gold) FROM graves), 0)::numeric
        <> coalesce((SELECT sum(quantity) FROM grave_inventory WHERE item_id = 'gold'), 0)::numeric THEN
        RAISE EXCEPTION 'gold balances were not preserved by the migration';
    END IF;
END;
$$;

DROP TRIGGER players_lifetime_gold ON players;
DROP FUNCTION track_lifetime_gold();
ALTER TABLE players DROP COLUMN gold;
ALTER TABLE graves DROP COLUMN gold;

-- Lifetime gold keeps counting every increase of the coin stack. New players
-- start at zero; the 100 starting coins are counted when they are granted.
ALTER TABLE players ALTER COLUMN lifetime_gold SET DEFAULT 0;

CREATE FUNCTION track_lifetime_gold() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    gained BIGINT;
BEGIN
    IF TG_OP = 'INSERT' THEN
        gained := NEW.quantity;
    ELSE
        gained := NEW.quantity - OLD.quantity;
    END IF;
    IF gained > 0 THEN
        UPDATE players SET lifetime_gold = lifetime_gold + gained WHERE id = NEW.player_id;
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER player_gold_lifetime AFTER INSERT OR UPDATE OF quantity ON player_inventory
    FOR EACH ROW WHEN (NEW.item_id = 'gold') EXECUTE FUNCTION track_lifetime_gold();

-- The starting 100 coins are granted once, when the player record is created.
CREATE OR REPLACE FUNCTION starter_inventory() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO player_inventory (player_id, item_id, quantity) VALUES
        (NEW.id, 'apple', 3), (NEW.id, 'bread', 3), (NEW.id, 'cheese', 2), (NEW.id, 'carrot', 2),
        (NEW.id, 'gold', 100);
    RETURN NEW;
END;
$$;
