-- Water that eating an item restores, in points of the 0-100 water reserve. Most food holds
-- a little; fruit and coconuts a good deal more than bread or cheese.
ALTER TABLE item_types
    ADD COLUMN water INTEGER NOT NULL DEFAULT 0 CHECK (water BETWEEN 0 AND 100);

UPDATE item_types SET water = CASE id
    WHEN 'apple' THEN 8
    WHEN 'pear' THEN 8
    WHEN 'carrot' THEN 5
    WHEN 'coconut' THEN 25
    WHEN 'cheese' THEN 1
    WHEN 'bread' THEN 1
    ELSE water END
WHERE id IN ('apple', 'pear', 'carrot', 'coconut', 'cheese', 'bread');
