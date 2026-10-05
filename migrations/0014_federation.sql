-- Federation foundations: world signing identity, pinned peer keys, and
-- player-initiated portal invitations and pacts. Operator policy decides
-- whether pacts are accepted automatically, wait for approval, or are refused.

CREATE TABLE federation_settings (
    world_id BIGINT PRIMARY KEY REFERENCES worlds(id),
    public_url TEXT CHECK (public_url ~ '^https?://[^[:space:]/?#@\\]+(/[^[:space:]?#@\\]*)?$' AND char_length(public_url) <= 300),
    policy TEXT NOT NULL DEFAULT 'approve' CHECK (policy IN ('closed', 'approve', 'open')),
    allow_private_peers BOOLEAN NOT NULL DEFAULT false,
    signing_key BYTEA NOT NULL CHECK (octet_length(signing_key) = 32),
    public_key BYTEA NOT NULL CHECK (octet_length(public_key) = 32),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE federation_peers (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    world_id BIGINT NOT NULL REFERENCES worlds(id),
    host TEXT NOT NULL CHECK (host ~ '^[a-z0-9]([a-z0-9.-]{0,251}[a-z0-9])?$'),
    api_url TEXT NOT NULL CHECK (char_length(api_url) <= 300),
    public_key BYTEA NOT NULL CHECK (octet_length(public_key) = 32),
    state TEXT NOT NULL DEFAULT 'trusted' CHECK (state IN ('trusted', 'banned')),
    first_seen TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (world_id, host)
);

CREATE TABLE portal_invitations (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    world_id BIGINT NOT NULL REFERENCES worlds(id),
    player_id BIGINT NOT NULL REFERENCES players(id),
    portal_name TEXT NOT NULL CHECK (portal_name ~ '^[a-z0-9-]{1,64}$'),
    expires_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    accepted_at TIMESTAMPTZ,
    accepted_by_host TEXT,
    accepted_by_player TEXT,
    revoked_at TIMESTAMPTZ,
    CHECK (expires_at > created_at),
    CHECK ((accepted_at IS NULL) = (accepted_by_host IS NULL))
);
CREATE INDEX portal_invitations_player ON portal_invitations (player_id) WHERE accepted_at IS NULL AND revoked_at IS NULL;

CREATE TABLE portal_pacts (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    world_id BIGINT NOT NULL REFERENCES worlds(id),
    invitation_id UUID NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('inviter', 'invitee')),
    player_id BIGINT NOT NULL REFERENCES players(id),
    peer_host TEXT NOT NULL CHECK (peer_host ~ '^[a-z0-9]([a-z0-9.-]{0,251}[a-z0-9])?$'),
    peer_player TEXT NOT NULL CHECK (char_length(peer_player) BETWEEN 1 AND 32),
    portal_name TEXT NOT NULL CHECK (portal_name ~ '^[a-z0-9-]{1,64}$'),
    peer_portal_name TEXT CHECK (peer_portal_name ~ '^[a-z0-9-]{1,64}$'),
    state TEXT NOT NULL DEFAULT 'proposed'
        CHECK (state IN ('proposed', 'accepted', 'declined', 'expired', 'building', 'open', 'closed', 'banned')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (world_id, invitation_id)
);
CREATE INDEX portal_pacts_player ON portal_pacts (player_id);
