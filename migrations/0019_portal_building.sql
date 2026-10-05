-- Joint building of a portal pact: each world builds its own end from materials the
-- player delivers to a construction site. The portal opens when both ends are built.

ALTER TABLE portal_pacts
    ADD COLUMN site_x DOUBLE PRECISION,
    ADD COLUMN site_y DOUBLE PRECISION,
    ADD COLUMN site_z DOUBLE PRECISION,
    ADD COLUMN portal_id BIGINT REFERENCES portals(id),
    ADD COLUMN local_built_at TIMESTAMPTZ,
    ADD COLUMN peer_built_at TIMESTAMPTZ,
    -- A status message ("built" or "closed") that the peer has not yet received.
    ADD COLUMN notify_pending TEXT CHECK (notify_pending IN ('built', 'closed')),
    ADD CONSTRAINT portal_pact_site_complete CHECK (
        (site_x IS NULL AND site_y IS NULL AND site_z IS NULL AND portal_id IS NULL) OR
        (site_x IS NOT NULL AND site_y IS NOT NULL AND site_z IS NOT NULL AND portal_id IS NOT NULL
         AND abs(site_x) < 1e12 AND abs(site_y) < 1e12 AND abs(site_z) < 1e12));

-- What has been delivered to the construction site, per item. The record stays after
-- the pact is closed: the ruin of a portal can later be mined for these materials.
CREATE TABLE portal_contributions (
    pact_id UUID NOT NULL REFERENCES portal_pacts(id),
    item_id TEXT NOT NULL REFERENCES item_types(id),
    quantity BIGINT NOT NULL CHECK (quantity > 0),
    PRIMARY KEY (pact_id, item_id)
);
