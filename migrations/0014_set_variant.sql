-- Grip or variant of a set ("neutral grip", "seated"): it changes the lift, so history compares per variant.
-- Free text; NULL = standard or unknown. Comparisons group by lower(variant).
ALTER TABLE workout_set ADD COLUMN variant text CHECK (btrim(variant) <> '');
