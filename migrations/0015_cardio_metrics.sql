-- Cardio metrics read off the machine's screen. All nullable; speeds are stored in km/h.
ALTER TABLE workout_set
    ADD COLUMN avg_hr integer CHECK (avg_hr BETWEEN 30 AND 250),
    ADD COLUMN max_hr integer CHECK (max_hr BETWEEN 30 AND 250),
    ADD COLUMN calories integer CHECK (calories BETWEEN 0 AND 10000),
    ADD COLUMN avg_incline_pct double precision CHECK (avg_incline_pct BETWEEN -10 AND 40),
    ADD COLUMN max_incline_pct double precision CHECK (max_incline_pct BETWEEN -10 AND 40),
    ADD COLUMN avg_speed_kmh double precision CHECK (avg_speed_kmh BETWEEN 0 AND 60),
    ADD COLUMN max_speed_kmh double precision CHECK (max_speed_kmh BETWEEN 0 AND 60),
    ADD CONSTRAINT workout_set_hr_order CHECK (max_hr >= avg_hr),
    ADD CONSTRAINT workout_set_incline_order CHECK (max_incline_pct >= avg_incline_pct),
    ADD CONSTRAINT workout_set_speed_order CHECK (max_speed_kmh >= avg_speed_kmh);
