-- Primary vs secondary muscle groups, and a finer list. Old coarse groups map in place:
-- back → lats + upper_back, shoulders → front_delts + side_delts; cardio is now exercise.kind (0005).
ALTER TABLE exercise DROP CONSTRAINT exercise_muscle_groups_check;

UPDATE exercise e SET muscle_groups = ARRAY(
    SELECT g FROM unnest(e.muscle_groups) WITH ORDINALITY AS m(name, i),
         LATERAL unnest(CASE m.name
                            WHEN 'back' THEN ARRAY['lats', 'upper_back']
                            WHEN 'shoulders' THEN ARRAY['front_delts', 'side_delts']
                            WHEN 'cardio' THEN ARRAY[]::text[]
                            ELSE ARRAY[m.name] END) WITH ORDINALITY AS x(g, j)
    GROUP BY g ORDER BY min(m.i * 10 + x.j));

ALTER TABLE exercise
    ADD COLUMN secondary_muscle_groups text[] NOT NULL DEFAULT '{}',
    ADD CONSTRAINT exercise_muscle_groups_check CHECK (
        muscle_groups <@ ARRAY['chest', 'lats', 'upper_back', 'lower_back', 'front_delts', 'side_delts', 'rear_delts',
                               'biceps', 'triceps', 'forearms', 'core', 'glutes', 'quads', 'hamstrings',
                               'adductors', 'abductors', 'calves', 'neck']
        AND secondary_muscle_groups <@ ARRAY['chest', 'lats', 'upper_back', 'lower_back', 'front_delts', 'side_delts',
                                             'rear_delts', 'biceps', 'triceps', 'forearms', 'core', 'glutes', 'quads',
                                             'hamstrings', 'adductors', 'abductors', 'calves', 'neck']
        AND NOT (muscle_groups && secondary_muscle_groups));
