-- Building, farming, cooking and fishing. Things that characters put into the world (a campfire,
-- a workbench, a tent, a growing crop) are rows of `placed_objects`: base terrain stays generated
-- and only these changes are stored, like harvested trees. A crop is a placed object that is ripe
-- from `ripe_at` on; the server computes growth from the clock, so offline time counts.

CREATE TABLE placed_objects (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    world_id BIGINT NOT NULL REFERENCES worlds(id),
    owner_id BIGINT REFERENCES players(id) ON DELETE SET NULL,
    kind TEXT NOT NULL CHECK (char_length(kind) <= 40),
    position_x DOUBLE PRECISION NOT NULL CHECK (abs(position_x) < 1e8),
    position_y DOUBLE PRECISION NOT NULL CHECK (abs(position_y) < 1e8),
    position_z DOUBLE PRECISION NOT NULL CHECK (abs(position_z) < 1e8),
    yaw DOUBLE PRECISION NOT NULL DEFAULT 0 CHECK (yaw >= 0 AND yaw <= 6.2832),
    planted_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    ripe_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX placed_objects_area ON placed_objects (world_id, position_x);
CREATE INDEX placed_objects_owner ON placed_objects (owner_id);

-- Things to put into the world are a category of their own.
ALTER TABLE item_types DROP CONSTRAINT item_types_category_check;
ALTER TABLE item_types ADD CONSTRAINT item_types_category_check
    CHECK (category IN ('food', 'raw_material', 'tool', 'weapon', 'armor', 'shield', 'consumable',
                        'container', 'valuable', 'key', 'document', 'remains', 'currency', 'animal', 'vehicle', 'placeable', 'misc'));

-- New foods, seeds, the fishing rod and things to put into the world.
INSERT INTO item_types (id, name, calories, capacity_bonus, stack_limit, multi_slot, slot_cost, category, water) VALUES
    ('fish', 'Fish', 250, 0, 20, false, 1, 'food', 5),
    ('cooked_fish', 'Cooked fish', 600, 0, 20, false, 1, 'food', 5),
    ('cooked_meat', 'Cooked meat', 800, 0, 20, false, 1, 'food', 2),
    ('corn', 'Corn', 350, 0, 50, false, 1, 'food', 3),
    ('cabbage', 'Cabbage', 150, 0, 50, false, 1, 'food', 10),
    ('pumpkin', 'Pumpkin', 300, 0, 20, false, 1, 'food', 6);
INSERT INTO item_types (id, name, calories, capacity_bonus, stack_limit, multi_slot, slot_cost, category) VALUES
    ('seed_carrot', 'Carrot seeds', 0, 0, 50, false, 1, 'raw_material'),
    ('seed_corn', 'Corn seeds', 0, 0, 50, false, 1, 'raw_material'),
    ('seed_cabbage', 'Cabbage seeds', 0, 0, 50, false, 1, 'raw_material'),
    ('seed_pumpkin', 'Pumpkin seeds', 0, 0, 50, false, 1, 'raw_material'),
    ('campfire', 'Campfire', 0, 0, 5, false, 2, 'placeable'),
    ('bedroll', 'Bedroll', 0, 0, 5, false, 2, 'placeable'),
    ('workbench', 'Workbench', 0, 0, 1, true, 4, 'placeable'),
    ('anvil', 'Anvil', 0, 0, 1, true, 4, 'placeable'),
    ('tent', 'Tent', 0, 0, 1, true, 6, 'placeable'),
    ('fence', 'Fence', 0, 0, 50, false, 1, 'placeable');
INSERT INTO item_types (id, name, calories, capacity_bonus, stack_limit, multi_slot, slot_cost, category, max_durability) VALUES
    ('fishing_rod', 'Fishing rod', 0, 0, 5, false, 1, 'tool', 120);
