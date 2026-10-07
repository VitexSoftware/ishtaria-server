-- Trading. Merchants (NPCs with a `shop:<id>` tag, goods in etc/shops.json) buy and sell for gold;
-- a daily limit of what one character can sell to a shop bounds the gold the world mints. Players
-- trade with each other through one open exchange at a time: both put items on the table,
-- both accept, and the swap happens in one transaction. An exchange is not history: when it ends
-- (done, cancelled, expired) it is deleted.

ALTER TABLE player_events
    DROP CONSTRAINT player_events_kind_check,
    ADD CONSTRAINT player_events_kind_check CHECK (kind IN ('level_up', 'say', 'whisper', 'trade'));

CREATE TABLE shop_sales (
    player_id BIGINT NOT NULL REFERENCES players(id) ON DELETE CASCADE,
    shop_id TEXT NOT NULL CHECK (char_length(shop_id) <= 64),
    window_start TIMESTAMPTZ NOT NULL DEFAULT now(),
    gold BIGINT NOT NULL CHECK (gold >= 0),
    PRIMARY KEY (player_id, shop_id)
);

CREATE TABLE trades (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    initiator_id BIGINT NOT NULL REFERENCES players(id) ON DELETE CASCADE,
    partner_id BIGINT NOT NULL REFERENCES players(id) ON DELETE CASCADE,
    initiator_accepted BOOLEAN NOT NULL DEFAULT false,
    partner_accepted BOOLEAN NOT NULL DEFAULT false,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (initiator_id <> partner_id)
);
CREATE UNIQUE INDEX trades_initiator ON trades (initiator_id);
CREATE UNIQUE INDEX trades_partner ON trades (partner_id);

CREATE TABLE trade_offers (
    trade_id BIGINT NOT NULL REFERENCES trades(id) ON DELETE CASCADE,
    player_id BIGINT NOT NULL REFERENCES players(id) ON DELETE CASCADE,
    item_id TEXT NOT NULL REFERENCES item_types(id),
    quantity BIGINT NOT NULL CHECK (quantity > 0),
    PRIMARY KEY (trade_id, player_id, item_id)
);
