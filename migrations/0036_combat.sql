-- Combat. Dangerous animals strike back when they are hit, armour and shields reduce what
-- reaches the character, and weapons and armour can be made from what hunting and mining give.

-- A character can be killed by a creature.
ALTER TABLE players
    DROP CONSTRAINT players_death_cause_check,
    ADD CONSTRAINT players_death_cause_check CHECK (death_cause IN ('starvation', 'disease', 'fall', 'falling_tree', 'hostile_player', 'exhaustion', 'dehydration', 'creature'));
ALTER TABLE graves
    DROP CONSTRAINT graves_cause_check,
    ADD CONSTRAINT graves_cause_check CHECK (cause IN ('starvation', 'disease', 'fall', 'falling_tree', 'hostile_player', 'exhaustion', 'dehydration', 'creature'));

-- Worn armour: the body and the hands (which armour goes where is in etc/combat.json).
ALTER TABLE player_equipment
    ADD COLUMN body TEXT REFERENCES item_types(id),
    ADD COLUMN hands TEXT REFERENCES item_types(id);

-- What hunting and smelting give, and what weapons and armour are made of.
INSERT INTO item_types (id, name, calories, capacity_bonus, stack_limit, multi_slot, slot_cost, category) VALUES
    ('hide', 'Hide', 0, 0, 20, false, 1, 'raw_material'),
    ('iron_ingot', 'Iron ingot', 0, 0, 30, false, 1, 'raw_material');
