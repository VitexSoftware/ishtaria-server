ALTER TABLE worlds ADD COLUMN atmosphere JSONB NOT NULL DEFAULT
 '{"skybox":"day","space_skybox":"galaxy","sun_color":[1.0,0.95,0.85],"sun_energy":1.2,"ambient_color":[0.7,0.78,0.84],"ambient_energy":0.45,"fog_color":[0.72,0.84,0.85],"fog_density":0.000025,"sky_energy":1.0}'::jsonb
CHECK (jsonb_typeof(atmosphere) = 'object'
    AND atmosphere ? 'skybox'
    AND atmosphere->>'skybox' IN ('day', 'morning', 'night', 'alien', 'space'));
