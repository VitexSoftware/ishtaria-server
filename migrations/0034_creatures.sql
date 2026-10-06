-- Animals that can be butchered for meat and cows that can be milked.
--
-- Raw meat is food like any other (eating it fills the food reserve). The animals themselves
-- stay generated: a butchered animal is stored as a change of the world in `world_object_state`
-- and returns after a while, exactly like a felled tree.

INSERT INTO item_types (id, name, calories, capacity_bonus, stack_limit, multi_slot, slot_cost, category) VALUES
    ('raw_meat', 'Raw meat', 400, 0, 20, false, 1, 'food');

-- When a cow can be milked again. Milk is drunk on the spot, so nothing is added to inventories.
CREATE TABLE creature_milked (
    world_id BIGINT NOT NULL REFERENCES worlds(id),
    object_id TEXT NOT NULL CHECK (char_length(object_id) <= 100),
    available_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (world_id, object_id)
);
