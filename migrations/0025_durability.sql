-- Durability. Tools, weapons, armour and shields wear out with use. A stack holds `quantity`
-- pieces of which one is in use; its remaining durability is stored with the stack and NULL
-- means the piece is new. When it reaches zero the piece is destroyed and the next one is new.
-- The off hand holds a shield, which blocks while the right mouse button is held.

ALTER TABLE item_types ADD COLUMN max_durability INTEGER CHECK (max_durability > 0);
UPDATE item_types SET max_durability = CASE category
    WHEN 'tool' THEN 120 WHEN 'weapon' THEN 150 WHEN 'armor' THEN 200 WHEN 'shield' THEN 150 END
    WHERE category IN ('tool', 'weapon', 'armor', 'shield');

ALTER TABLE player_inventory ADD COLUMN durability INTEGER CHECK (durability > 0);
ALTER TABLE grave_inventory ADD COLUMN durability INTEGER CHECK (durability > 0);

ALTER TABLE player_equipment ALTER COLUMN hand DROP NOT NULL;
ALTER TABLE player_equipment ADD COLUMN offhand TEXT REFERENCES item_types(id);

ALTER TABLE players ADD COLUMN blocked_until TIMESTAMPTZ;
