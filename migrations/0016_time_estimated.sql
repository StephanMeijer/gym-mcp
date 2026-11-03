-- A placeholder time ("around noon", until the real one is known); cleared when the real time is set.
ALTER TABLE workout ADD COLUMN time_estimated boolean NOT NULL DEFAULT false;
ALTER TABLE workout_set ADD COLUMN time_estimated boolean NOT NULL DEFAULT false;
