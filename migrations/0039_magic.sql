-- Magic (ADR 0010). Mana is a reserve that regenerates by the clock: it is stored with the moment
-- it was last changed and read as `least(max, mana + seconds * rate)`, so there is no tick loop
-- and offline time counts. Spells are learned from scrolls and kept for good.

ALTER TABLE players
    ADD COLUMN mana DOUBLE PRECISION NOT NULL DEFAULT 100 CHECK (mana >= 0 AND mana <= 100),
    ADD COLUMN mana_updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    ADD COLUMN warded_until TIMESTAMPTZ;

CREATE TABLE player_spells (
    player_id BIGINT NOT NULL REFERENCES players(id) ON DELETE CASCADE,
    spell_id TEXT NOT NULL CHECK (char_length(spell_id) <= 40),
    learned_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    cooldown_until TIMESTAMPTZ,
    PRIMARY KEY (player_id, spell_id)
);

INSERT INTO item_types (id, name, calories, capacity_bonus, stack_limit, multi_slot, slot_cost, category) VALUES
    ('scroll_refresh', 'Scroll of Refresh', 0, 0, 5, false, 1, 'document'),
    ('scroll_firebolt', 'Scroll of Firebolt', 0, 0, 5, false, 1, 'document'),
    ('scroll_heal', 'Scroll of Healing', 0, 0, 5, false, 1, 'document'),
    ('scroll_ward', 'Scroll of Ward', 0, 0, 5, false, 1, 'document');
