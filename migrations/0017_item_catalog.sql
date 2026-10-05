-- Item categories and the RPG item catalog (Quaternius Ultimate RPG Items Bundle,
-- CC0). The items have names, categories and icons; they have no statistics yet
-- and no way of being obtained until combat, smithing and loot exist.

ALTER TABLE item_types ADD COLUMN category TEXT NOT NULL DEFAULT 'misc'
    CHECK (category IN ('food', 'raw_material', 'tool', 'weapon', 'armor', 'shield', 'consumable',
                        'container', 'valuable', 'key', 'document', 'remains', 'currency', 'misc'));

UPDATE item_types SET category = 'food' WHERE id IN ('apple', 'bread', 'cheese', 'carrot', 'pear', 'acorn', 'pine_nut', 'coconut');
UPDATE item_types SET category = 'raw_material' WHERE id IN
    ('pine_log', 'oak_log', 'palm_log', 'birch_log', 'pine_wood', 'oak_wood', 'palm_wood', 'birch_wood',
     'pine_plank', 'oak_plank', 'palm_plank', 'birch_plank', 'stone', 'stone_block', 'iron_ore', 'copper_ore', 'quartz_crystal');
UPDATE item_types SET category = 'tool' WHERE id IN ('axe', 'pickaxe');
UPDATE item_types SET category = 'weapon' WHERE id = 'sword';
UPDATE item_types SET category = 'container' WHERE id IN ('bag', 'suitcase');
UPDATE item_types SET category = 'currency' WHERE id = 'gold';

INSERT INTO item_types (id, name, calories, capacity_bonus, stack_limit, multi_slot, slot_cost, category) VALUES
    ('armor_golden', 'Armor Golden', 0, 0, 1, true, 1, 'armor'),
    ('armor_leather', 'Armor Leather', 0, 0, 1, true, 1, 'armor'),
    ('armor_metal', 'Armor Metal', 0, 0, 1, true, 1, 'armor'),
    ('arrow', 'Arrow', 0, 0, 50, true, 1, 'weapon'),
    ('axe_double', 'Axe Double', 0, 0, 1, true, 1, 'weapon'),
    ('axe_small', 'Axe Small', 0, 0, 1, true, 1, 'weapon'),
    ('backpack', 'Backpack', 0, 0, 1, true, 1, 'container'),
    ('bone', 'Bone', 0, 0, 20, false, 1, 'remains'),
    ('open_book', 'Open Book', 0, 0, 1, true, 1, 'document'),
    ('open_book_2', 'Open Book II', 0, 0, 1, true, 1, 'document'),
    ('open_book_3', 'Open Book III', 0, 0, 1, true, 1, 'document'),
    ('open_book_4', 'Open Book IV', 0, 0, 1, true, 1, 'document'),
    ('book', 'Book', 0, 0, 1, true, 1, 'document'),
    ('book_2', 'Book II', 0, 0, 1, true, 1, 'document'),
    ('book_3', 'Book III', 0, 0, 1, true, 1, 'document'),
    ('chalice', 'Chalice', 0, 0, 1, true, 1, 'valuable'),
    ('chest', 'Chest', 0, 0, 1, true, 1, 'container'),
    ('claymore', 'Claymore', 0, 0, 1, true, 1, 'weapon'),
    ('coin_pouch', 'Coin Pouch', 0, 0, 1, true, 1, 'container'),
    ('crown', 'Crown', 0, 0, 1, true, 1, 'valuable'),
    ('dagger', 'Dagger', 0, 0, 1, true, 1, 'weapon'),
    ('doublesided_hammer', 'Doublesided Hammer', 0, 0, 1, true, 1, 'weapon'),
    ('fish_bone', 'Fish Bone', 0, 0, 20, false, 1, 'remains'),
    ('glove', 'Glove', 0, 0, 1, true, 1, 'armor'),
    ('gold_ingots', 'Gold Ingots', 0, 0, 10, true, 1, 'valuable'),
    ('key', 'Key', 0, 0, 1, true, 1, 'key'),
    ('key_2', 'Key II', 0, 0, 1, true, 1, 'key'),
    ('key_3', 'Key III', 0, 0, 1, true, 1, 'key'),
    ('key_4', 'Key IV', 0, 0, 1, true, 1, 'key'),
    ('knife', 'Knife', 0, 0, 1, true, 1, 'weapon'),
    ('mineral', 'Mineral', 0, 0, 20, false, 1, 'valuable'),
    ('necklace', 'Necklace', 0, 0, 1, true, 1, 'valuable'),
    ('necklace_2', 'Necklace II', 0, 0, 1, true, 1, 'valuable'),
    ('padlock', 'Padlock', 0, 0, 1, true, 1, 'key'),
    ('parchment', 'Parchment', 0, 0, 1, true, 1, 'document'),
    ('potion_bottle', 'Potion Bottle', 0, 0, 20, false, 1, 'consumable'),
    ('potion_bottle_2', 'Potion Bottle II', 0, 0, 20, false, 1, 'consumable'),
    ('scroll', 'Scroll', 0, 0, 1, true, 1, 'document'),
    ('scythe', 'Scythe', 0, 0, 1, true, 1, 'weapon'),
    ('shield_celtic_golden', 'Shield Celtic Golden', 0, 0, 1, true, 1, 'shield'),
    ('shield_heater', 'Shield Heater', 0, 0, 1, true, 1, 'shield'),
    ('shield_heater_2', 'Shield Heater II', 0, 0, 1, true, 1, 'shield'),
    ('shield_round', 'Shield Round', 0, 0, 1, true, 1, 'shield'),
    ('shield_round_2', 'Shield Round II', 0, 0, 1, true, 1, 'shield'),
    ('skull_coin', 'Skull Coin', 0, 0, 1, true, 1, 'valuable'),
    ('skull', 'Skull', 0, 0, 1, true, 1, 'remains'),
    ('skull_2', 'Skull II', 0, 0, 1, true, 1, 'remains'),
    ('snowflake', 'Snowflake', 0, 0, 20, false, 1, 'misc'),
    ('spear', 'Spear', 0, 0, 1, true, 1, 'weapon'),
    ('star_coin', 'Star Coin', 0, 0, 1, true, 1, 'valuable'),
    ('sword_2', 'Sword II', 0, 0, 1, true, 1, 'weapon'),
    ('wooden_bow', 'Wooden Bow', 0, 0, 1, true, 1, 'weapon');
