-- Which machine (brand/model) a set was done on: machine weights only compare within one brand.
-- Free text; NULL = unknown. Comparisons group by lower(machine).
ALTER TABLE workout_set ADD COLUMN machine text CHECK (btrim(machine) <> '');
CREATE INDEX workout_set_machine ON workout_set (exercise_id, lower(machine), performed_at);
