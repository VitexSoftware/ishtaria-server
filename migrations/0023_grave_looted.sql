-- A grave that has been robbed no longer counts for the wealth of its character in the
-- hall of fame. The first item taken from a grave records the moment.
ALTER TABLE graves ADD COLUMN looted_at TIMESTAMPTZ;

-- Graves that were already emptied before this record existed count as robbed.
UPDATE graves SET looted_at = now()
    WHERE NOT EXISTS (SELECT 1 FROM grave_inventory WHERE grave_inventory.grave_id = graves.id);
