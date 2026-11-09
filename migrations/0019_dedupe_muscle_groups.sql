-- A muscle listed twice in one exercise counted twice in muscle volume; the app now rejects that (#88).
-- Remove existing duplicates, keeping first-seen order.
UPDATE exercise e SET
    muscle_groups = ARRAY(SELECT g FROM unnest(e.muscle_groups) WITH ORDINALITY AS m(g, i) GROUP BY g ORDER BY min(i)),
    secondary_muscle_groups =
        ARRAY(SELECT g FROM unnest(e.secondary_muscle_groups) WITH ORDINALITY AS m(g, i) GROUP BY g ORDER BY min(i))
WHERE cardinality(e.muscle_groups) <> (SELECT count(DISTINCT g) FROM unnest(e.muscle_groups) g)
   OR cardinality(e.secondary_muscle_groups) <> (SELECT count(DISTINCT g) FROM unnest(e.secondary_muscle_groups) g);
