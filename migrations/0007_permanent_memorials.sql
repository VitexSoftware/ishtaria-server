CREATE FUNCTION preserve_memorial() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'grave memorials are permanent';
END;
$$;

CREATE TRIGGER permanent_grave_memorial
BEFORE DELETE OR UPDATE OF id, world_id, player_id, player_uuid, player_name,
    died_at, cause, kind, lifetime_gold, position_x, position_y, position_z,
    lived_days, friends_count ON graves
FOR EACH ROW EXECUTE FUNCTION preserve_memorial();