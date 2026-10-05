ALTER TABLE players
    ADD COLUMN uuid UUID NOT NULL DEFAULT gen_random_uuid() UNIQUE,
    ADD COLUMN lifetime_gold BIGINT NOT NULL DEFAULT 100 CHECK (lifetime_gold >= 0),
    ADD COLUMN last_ate_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    ADD COLUMN calories_consumed BIGINT NOT NULL DEFAULT 0 CHECK (calories_consumed >= 0),
    ADD COLUMN died_at TIMESTAMPTZ,
    ADD COLUMN death_cause TEXT CHECK (death_cause IN ('starvation', 'disease', 'fall', 'falling_tree', 'hostile_player')),
    ADD COLUMN position_x DOUBLE PRECISION,
    ADD COLUMN position_y DOUBLE PRECISION,
    ADD COLUMN position_z DOUBLE PRECISION,
    ADD CONSTRAINT player_position_complete CHECK (
        (position_x IS NULL AND position_y IS NULL AND position_z IS NULL) OR
        (position_x IS NOT NULL AND position_y IS NOT NULL AND position_z IS NOT NULL
        AND abs(position_x) < 1e12 AND abs(position_y) < 1e12 AND abs(position_z) < 1e12)),
    ADD CONSTRAINT player_death_complete CHECK ((died_at IS NULL) = (death_cause IS NULL));

UPDATE players SET lifetime_gold = greatest(gold, 100);

CREATE FUNCTION track_lifetime_gold() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.gold > OLD.gold THEN
        NEW.lifetime_gold := OLD.lifetime_gold + (NEW.gold - OLD.gold);
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER players_lifetime_gold BEFORE UPDATE OF gold ON players
    FOR EACH ROW EXECUTE FUNCTION track_lifetime_gold();

CREATE TABLE item_types (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    calories INTEGER NOT NULL DEFAULT 0 CHECK (calories BETWEEN 0 AND 10000),
    capacity_bonus INTEGER NOT NULL DEFAULT 0 CHECK (capacity_bonus BETWEEN 0 AND 100),
    stack_limit BIGINT NOT NULL CHECK (stack_limit > 0),
    asset TEXT
);

INSERT INTO item_types (id, name, calories, capacity_bonus, stack_limit) VALUES
    ('apple', 'Apple', 95, 0, 100),
    ('bread', 'Bread', 265, 0, 100),
    ('cheese', 'Cheese', 113, 0, 100),
    ('carrot', 'Carrot', 25, 0, 100),
    ('bag', 'Bag', 0, 20, 1),
    ('suitcase', 'Suitcase', 0, 50, 1);

CREATE TABLE player_inventory (
    player_id BIGINT NOT NULL REFERENCES players(id) ON DELETE CASCADE,
    item_id TEXT NOT NULL REFERENCES item_types(id),
    quantity BIGINT NOT NULL CHECK (quantity > 0),
    PRIMARY KEY (player_id, item_id)
);

CREATE FUNCTION starter_inventory() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO player_inventory (player_id, item_id, quantity) VALUES
        (NEW.id, 'apple', 3), (NEW.id, 'bread', 3), (NEW.id, 'cheese', 2), (NEW.id, 'carrot', 2);
    RETURN NEW;
END;
$$;
CREATE TRIGGER players_starter_inventory AFTER INSERT ON players
    FOR EACH ROW EXECUTE FUNCTION starter_inventory();

CREATE TABLE graves (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    world_id BIGINT NOT NULL REFERENCES worlds(id),
    player_id BIGINT NOT NULL UNIQUE REFERENCES players(id),
    player_uuid UUID NOT NULL,
    player_name TEXT NOT NULL,
    died_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    cause TEXT NOT NULL CHECK (cause IN ('starvation', 'disease', 'fall', 'falling_tree', 'hostile_player')),
    kind TEXT NOT NULL CHECK (kind IN ('headstone', 'monument', 'mausoleum')),
    lifetime_gold BIGINT NOT NULL CHECK (lifetime_gold >= 0),
    gold BIGINT NOT NULL CHECK (gold >= 0),
    position_x DOUBLE PRECISION,
    position_y DOUBLE PRECISION,
    position_z DOUBLE PRECISION
);
CREATE INDEX graves_world ON graves (world_id);

CREATE TABLE grave_inventory (
    grave_id BIGINT NOT NULL REFERENCES graves(id),
    item_id TEXT NOT NULL REFERENCES item_types(id),
    quantity BIGINT NOT NULL CHECK (quantity > 0),
    PRIMARY KEY (grave_id, item_id)
);