-- Cardio: an exercise kind, plus distance and free-text intensity per set ("level 12, incline 6%";
-- units differ per machine). The old 'cardio' muscle group seeds the kind.
ALTER TABLE exercise ADD COLUMN kind text NOT NULL DEFAULT 'strength' CHECK (kind IN ('strength', 'cardio'));
UPDATE exercise SET kind = 'cardio' WHERE 'cardio' = ANY(muscle_groups);

ALTER TABLE workout_set
    ADD COLUMN distance_m double precision CHECK (distance_m >= 0),
    ADD COLUMN intensity text NOT NULL DEFAULT '';
