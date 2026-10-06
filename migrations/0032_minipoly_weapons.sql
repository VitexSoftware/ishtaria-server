-- Weapons of the MiniPoly Weapons bundle on Poly Pizza (CC0 and CC-BY 3.0). Like the other
-- weapons they have names, a category and durability but no statistics yet, and no way of
-- being obtained until combat, smithing and loot exist.

INSERT INTO item_types (id, name, calories, capacity_bonus, stack_limit, multi_slot, slot_cost, category, max_durability) VALUES
    ('axe_2', 'Axe II', 0, 0, 1, true, 1, 'weapon', 150),
    ('axe_3', 'Axe III', 0, 0, 1, true, 1, 'weapon', 150),
    ('cleaver', 'Cleaver', 0, 0, 1, true, 1, 'weapon', 150),
    ('dagger_2', 'Dagger II', 0, 0, 1, true, 1, 'weapon', 150),
    ('trident', 'Trident', 0, 0, 1, true, 1, 'weapon', 150),
    ('devils_axe', 'Devil''s Axe', 0, 0, 1, true, 1, 'weapon', 150),
    ('talwar', 'Talwar', 0, 0, 1, true, 1, 'weapon', 150),
    ('devils_sword', 'Devil''s Sword', 0, 0, 1, true, 1, 'weapon', 150);
