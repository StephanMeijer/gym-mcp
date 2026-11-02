-- Warm-ups and cut-short sets are kept but only working sets count towards volume, progress, PRs and last_session.
ALTER TABLE workout_set ADD COLUMN set_type text NOT NULL DEFAULT 'working'
    CHECK (set_type IN ('warmup', 'working', 'partial', 'interrupted'));
