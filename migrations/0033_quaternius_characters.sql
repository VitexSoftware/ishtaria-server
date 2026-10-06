-- Playable characters of Quaternius (CC0 and CC BY 3.0 glTF models with their own animations)
-- join the Kenney packs. The default stays `retro/humanMaleA`.

ALTER TABLE players DROP CONSTRAINT players_character_check;
ALTER TABLE players ADD CONSTRAINT players_character_check
    CHECK (character IN (
        'protagonists/criminalMaleA', 'protagonists/cyborgFemaleA',
        'protagonists/skaterFemaleA', 'protagonists/skaterMaleA',
        'retro/humanFemaleA', 'retro/humanMaleA',
        'retro/zombieFemaleA', 'retro/zombieMaleA',
        'survivors/survivorFemaleA', 'survivors/survivorMaleB',
        'survivors/zombieA', 'survivors/zombieC',
        'quaternius/adventurer', 'quaternius/adventurer_woman',
        'quaternius/hooded_adventurer_woman', 'quaternius/character_animated',
        'quaternius/hoodie_character', 'quaternius/punk',
        'quaternius/punk_woman', 'quaternius/animated_woman',
        'quaternius/animated_woman_2', 'quaternius/suit_woman',
        'quaternius/worker_woman', 'quaternius/soldier_woman',
        'quaternius/sci_fi_woman', 'quaternius/witch'
    ));
