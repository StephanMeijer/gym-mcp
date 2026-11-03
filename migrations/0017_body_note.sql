-- Dated aches, pain and fatigue, to spot patterns over weeks.
CREATE TABLE body_note (
    id         bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    owner_sub  text NOT NULL,
    noted_at   timestamptz NOT NULL,
    location   text NOT NULL CHECK (location <> ''),
    severity   smallint CHECK (severity BETWEEN 1 AND 10),
    notes      text NOT NULL DEFAULT '',
    workout_id bigint REFERENCES workout (id) ON DELETE SET NULL
);
CREATE INDEX body_note_owner_at ON body_note (owner_sub, noted_at);
