ALTER TABLE players
    ADD COLUMN survival_updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    ADD COLUMN last_active_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    ADD COLUMN activity_seconds DOUBLE PRECISION NOT NULL DEFAULT 0 CHECK (activity_seconds BETWEEN 0 AND 10),
    ADD COLUMN stamina_fraction DOUBLE PRECISION NOT NULL DEFAULT 0 CHECK (stamina_fraction >= 0 AND stamina_fraction < 1),
    ADD COLUMN water_fraction DOUBLE PRECISION NOT NULL DEFAULT 0 CHECK (water_fraction >= 0 AND water_fraction < 1),
    ADD COLUMN health_fraction DOUBLE PRECISION NOT NULL DEFAULT 0 CHECK (health_fraction >= 0 AND health_fraction < 1),
    DROP CONSTRAINT players_death_cause_check,
    ADD CONSTRAINT players_death_cause_check CHECK (death_cause IN ('starvation', 'disease', 'fall', 'falling_tree', 'hostile_player', 'exhaustion', 'dehydration'));

ALTER TABLE graves
    DROP CONSTRAINT graves_cause_check,
    ADD CONSTRAINT graves_cause_check CHECK (cause IN ('starvation', 'disease', 'fall', 'falling_tree', 'hostile_player', 'exhaustion', 'dehydration'));