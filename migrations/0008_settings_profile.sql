-- More context for recommendations than the one-word goal: free-text goals and plans (events and
-- deadlines go here too), muscle groups to prioritise, and injuries/limitations to work around.
ALTER TABLE user_settings
    ADD COLUMN goals_notes text,
    ADD COLUMN priority_muscle_groups text[],
    ADD COLUMN limitations text;
