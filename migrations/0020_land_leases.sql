-- Real-money land leases. The operator rents out rectangles of map tiles (one tile is
-- one cell of the object grid, about 17 m). Payments arrive as orders confirmed by a
-- signed webhook of the payment provider; nothing here handles card data.

CREATE TABLE monetization_settings (
    world_id BIGINT PRIMARY KEY REFERENCES worlds(id),
    enabled BOOLEAN NOT NULL DEFAULT false,
    provider TEXT NOT NULL DEFAULT 'manual' CHECK (provider IN ('manual')),
    currency TEXT NOT NULL DEFAULT 'CZK' CHECK (currency ~ '^[A-Z]{3}$'),
    tile_price_minor BIGINT NOT NULL DEFAULT 1000 CHECK (tile_price_minor BETWEEN 1 AND 100000000),
    model_price_minor BIGINT NOT NULL DEFAULT 10000 CHECK (model_price_minor BETWEEN 1 AND 100000000),
    avatar_price_minor BIGINT NOT NULL DEFAULT 20000 CHECK (avatar_price_minor BETWEEN 1 AND 100000000),
    grace_days INTEGER NOT NULL DEFAULT 7 CHECK (grace_days BETWEEN 0 AND 90),
    max_tiles INTEGER NOT NULL DEFAULT 400 CHECK (max_tiles BETWEEN 1 AND 10000),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE land_leases (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    world_id BIGINT NOT NULL REFERENCES worlds(id),
    player_id BIGINT NOT NULL REFERENCES players(id),
    face INTEGER NOT NULL CHECK (face BETWEEN 0 AND 5),
    x0 INTEGER NOT NULL CHECK (x0 >= 0),
    y0 INTEGER NOT NULL CHECK (y0 >= 0),
    x1 INTEGER NOT NULL,
    y1 INTEGER NOT NULL,
    -- The price per tile and month agreed when the lease was made.
    tile_price_minor BIGINT NOT NULL CHECK (tile_price_minor > 0),
    currency TEXT NOT NULL CHECK (currency ~ '^[A-Z]{3}$'),
    -- NULL until the first payment arrives.
    paid_until TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    released_at TIMESTAMPTZ,
    CHECK (x1 >= x0 AND y1 >= y0)
);
CREATE INDEX land_leases_area ON land_leases (world_id, face) WHERE released_at IS NULL;
CREATE INDEX land_leases_player ON land_leases (player_id) WHERE released_at IS NULL;

CREATE TABLE payment_orders (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    world_id BIGINT NOT NULL REFERENCES worlds(id),
    player_id BIGINT NOT NULL REFERENCES players(id),
    kind TEXT NOT NULL CHECK (kind IN ('lease_new', 'lease_renew')),
    lease_id UUID REFERENCES land_leases(id),
    amount_minor BIGINT NOT NULL CHECK (amount_minor > 0),
    currency TEXT NOT NULL CHECK (currency ~ '^[A-Z]{3}$'),
    status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'paid', 'failed', 'refunded')),
    provider TEXT NOT NULL,
    provider_ref TEXT UNIQUE CHECK (char_length(provider_ref) <= 200),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK ((kind IN ('lease_new', 'lease_renew')) = (lease_id IS NOT NULL))
);
CREATE INDEX payment_orders_player ON payment_orders (player_id);
