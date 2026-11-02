-- Effort and rest per set, and a training goal per user, so recommendations can reason
-- about progression ("3x10 at RPE 7 last time → add weight") instead of weights alone.
ALTER TABLE workout_set
    ADD COLUMN rpe double precision CHECK (rpe BETWEEN 1 AND 10 AND rpe * 2 = round(rpe * 2)),
    ADD COLUMN rest_s integer CHECK (rest_s >= 0);

ALTER TABLE user_settings
    ADD COLUMN goal text CHECK (goal IN ('strength', 'hypertrophy', 'endurance', 'general'));
