-- A portal is built on its own and, once complete, linked with a finished portal of another
-- world by a share link. This replaces invitations and pacts: a portal record no longer needs
-- an invitation or a peer until it is linked.
ALTER TABLE portal_pacts
    ALTER COLUMN invitation_id DROP NOT NULL,
    ALTER COLUMN peer_host DROP NOT NULL,
    ALTER COLUMN peer_player DROP NOT NULL,
    ADD COLUMN peer_portal_id UUID;

ALTER TABLE portal_pacts DROP CONSTRAINT portal_pacts_role_check;
ALTER TABLE portal_pacts ADD CONSTRAINT portal_pacts_role_check
    CHECK (role IN ('inviter', 'invitee', 'builder'));
ALTER TABLE portal_pacts DROP CONSTRAINT portal_pacts_state_check;
-- built: complete and waiting for a link; pending: linked, waiting for the operator's approval.
ALTER TABLE portal_pacts ADD CONSTRAINT portal_pacts_state_check
    CHECK (state IN ('proposed', 'accepted', 'declined', 'expired', 'building', 'built', 'pending',
                     'open', 'closed', 'banned'));

-- A broken link the other world has not been told about yet; sent again until it arrives.
CREATE TABLE portal_unlinks (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    world_id BIGINT NOT NULL REFERENCES worlds(id),
    own_portal_id UUID NOT NULL,
    peer_host TEXT NOT NULL CHECK (peer_host ~ '^[a-z0-9]([a-z0-9.-]{0,251}[a-z0-9])?$'),
    peer_portal_id UUID NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
