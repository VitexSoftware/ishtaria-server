ALTER TABLE players ADD COLUMN character TEXT NOT NULL DEFAULT 'retro/humanMaleA'
    CHECK (character IN (
        'protagonists/criminalMaleA', 'protagonists/cyborgFemaleA',
        'protagonists/skaterFemaleA', 'protagonists/skaterMaleA',
        'retro/humanFemaleA', 'retro/humanMaleA',
        'retro/zombieFemaleA', 'retro/zombieMaleA',
        'survivors/survivorFemaleA', 'survivors/survivorMaleB',
        'survivors/zombieA', 'survivors/zombieC'
    ));