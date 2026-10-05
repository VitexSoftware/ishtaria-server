-- Gathering and crafting: harvested objects are persisted as changes of the
-- deterministic generated world; new raw materials, planks and foods.

CREATE TABLE world_object_state (
    world_id BIGINT NOT NULL REFERENCES worlds(id),
    object_id TEXT NOT NULL CHECK (char_length(object_id) <= 100),
    hits INTEGER NOT NULL DEFAULT 0 CHECK (hits >= 0),
    last_hit_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    depleted_until TIMESTAMPTZ,
    PRIMARY KEY (world_id, object_id)
);
CREATE INDEX world_object_state_depleted ON world_object_state (world_id, depleted_until) WHERE depleted_until IS NOT NULL;

ALTER TABLE players ADD COLUMN last_gathered_at TIMESTAMPTZ;

-- Some items are heavy: every stack occupies several inventory slots.
ALTER TABLE item_types ADD COLUMN slot_cost INTEGER NOT NULL DEFAULT 1 CHECK (slot_cost BETWEEN 1 AND 100);

INSERT INTO item_types (id, name, calories, capacity_bonus, stack_limit, multi_slot, slot_cost) VALUES
    -- A felled tree is a log that fills ten slots and must be chopped into wood.
    -- Tools are inventory items; every character starts with the basic ones.
    ('axe', 'Axe', 0, 0, 5, false, 1),
    ('pickaxe', 'Pickaxe', 0, 0, 5, false, 1),
    ('sword', 'Sword', 0, 0, 5, false, 1),
    ('pine_log', 'Pine log', 0, 0, 1, true, 10),
    ('oak_log', 'Oak log', 0, 0, 1, true, 10),
    ('palm_log', 'Palm log', 0, 0, 1, true, 10),
    ('birch_log', 'Birch log', 0, 0, 1, true, 10),
    ('pine_wood', 'Pine wood', 0, 0, 100, false, 1),
    ('oak_wood', 'Oak wood', 0, 0, 100, false, 1),
    ('palm_wood', 'Palm wood', 0, 0, 100, false, 1),
    ('birch_wood', 'Birch wood', 0, 0, 100, false, 1),
    ('pine_plank', 'Pine plank', 0, 0, 50, false, 1),
    ('oak_plank', 'Oak plank', 0, 0, 50, false, 1),
    ('palm_plank', 'Palm plank', 0, 0, 50, false, 1),
    ('birch_plank', 'Birch plank', 0, 0, 50, false, 1),
    ('stone', 'Stone', 0, 0, 50, false, 1),
    ('stone_block', 'Stone block', 0, 0, 20, false, 1),
    ('iron_ore', 'Iron ore', 0, 0, 30, false, 1),
    ('copper_ore', 'Copper ore', 0, 0, 30, false, 1),
    ('quartz_crystal', 'Quartz crystal', 0, 0, 20, false, 1),
    ('pear', 'Pear', 100, 0, 100, false, 1),
    ('acorn', 'Acorn', 110, 0, 100, false, 1),
    ('pine_nut', 'Pine nut', 190, 0, 100, false, 1),
    ('coconut', 'Coconut', 354, 0, 100, false, 1);

CREATE OR REPLACE FUNCTION starter_inventory() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO player_inventory (player_id, item_id, quantity) VALUES
        (NEW.id, 'apple', 3), (NEW.id, 'bread', 3), (NEW.id, 'cheese', 2), (NEW.id, 'carrot', 2),
        (NEW.id, 'gold', 100),
        (NEW.id, 'axe', 1), (NEW.id, 'pickaxe', 1), (NEW.id, 'sword', 1);
    RETURN NEW;
END;
$$;

-- Living characters created earlier receive the basic tools as well.
INSERT INTO player_inventory (player_id, item_id, quantity)
    SELECT players.id, tool.id, 1 FROM players CROSS JOIN (VALUES ('axe'), ('pickaxe'), ('sword')) AS tool(id)
    WHERE players.died_at IS NULL
    ON CONFLICT (player_id, item_id) DO NOTHING;
