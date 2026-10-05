ALTER TABLE players
    DROP CONSTRAINT players_jump_x_check,
    DROP CONSTRAINT players_jump_y_check,
    DROP CONSTRAINT players_jump_z_check,
    ADD CONSTRAINT players_jump_x_check CHECK (jump_x BETWEEN -6.01 AND 6.01),
    ADD CONSTRAINT players_jump_y_check CHECK (jump_y BETWEEN -6.01 AND 6.01),
    ADD CONSTRAINT players_jump_z_check CHECK (jump_z BETWEEN -6.01 AND 6.01);