-- Boats and ships sold by the shipwrights of coastal towns (Kenney Pirate Kit models).
-- A vessel is a single item for now; sailing does not exist yet.
ALTER TABLE item_types DROP CONSTRAINT item_types_category_check;
ALTER TABLE item_types ADD CONSTRAINT item_types_category_check
    CHECK (category IN ('food', 'raw_material', 'tool', 'weapon', 'armor', 'shield', 'consumable',
                        'container', 'valuable', 'key', 'document', 'remains', 'currency', 'animal', 'vehicle', 'misc'));

INSERT INTO item_types (id, name, calories, capacity_bonus, stack_limit, multi_slot, slot_cost, category) VALUES
    ('boat_row_small', 'Small rowing boat', 0, 0, 1, true, 10, 'vehicle'),
    ('boat_row_large', 'Large rowing boat', 0, 0, 1, true, 14, 'vehicle'),
    ('ship_small', 'Small ship', 0, 0, 1, true, 25, 'vehicle'),
    ('ship_medium', 'Medium ship', 0, 0, 1, true, 40, 'vehicle'),
    ('ship_large', 'Large ship', 0, 0, 1, true, 60, 'vehicle');
