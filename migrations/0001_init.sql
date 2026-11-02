-- Plain Postgres, no extensions. Everything is scoped by owner_sub (the OAuth `sub`).

CREATE TABLE user_settings (
    owner_sub text PRIMARY KEY,
    timezone  text NOT NULL
);

CREATE TABLE equipment (
    id        bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    owner_sub text NOT NULL,
    name      text NOT NULL CHECK (btrim(name) <> ''),
    aliases   text[] NOT NULL DEFAULT '{}',
    category  text NOT NULL DEFAULT 'accessory'
              CHECK (category IN ('machine', 'free_weight', 'rack_bench', 'cardio', 'accessory'))
);
CREATE UNIQUE INDEX equipment_owner_name ON equipment (owner_sub, lower(name));

CREATE TABLE gym (
    id                 bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    owner_sub          text NOT NULL,
    name               text NOT NULL CHECK (btrim(name) <> ''),
    aliases            text[] NOT NULL DEFAULT '{}',
    notes              text NOT NULL DEFAULT '',
    inventory_complete boolean NOT NULL DEFAULT false,
    created_at         timestamptz NOT NULL DEFAULT now(),
    notes_tsv          tsvector GENERATED ALWAYS AS (to_tsvector('simple', notes)) STORED
);
CREATE UNIQUE INDEX gym_owner_name ON gym (owner_sub, lower(name));
CREATE INDEX gym_notes_tsv ON gym USING gin (notes_tsv);

-- No row for (gym, equipment) means "unknown"; a row records a confirmed presence or absence.
CREATE TABLE gym_equipment (
    gym_id               bigint NOT NULL REFERENCES gym (id) ON DELETE CASCADE,
    equipment_id         bigint NOT NULL REFERENCES equipment (id) ON DELETE CASCADE,
    status               text NOT NULL CHECK (status IN ('present', 'absent')),
    quantity             integer CHECK (quantity > 0),
    available_weights_kg double precision[],
    max_load_kg          double precision CHECK (max_load_kg > 0),
    notes                text NOT NULL DEFAULT '',
    verified_at          timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (gym_id, equipment_id)
);

CREATE TABLE exercise (
    id            bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    owner_sub     text NOT NULL,
    name          text NOT NULL CHECK (btrim(name) <> ''),
    aliases       text[] NOT NULL DEFAULT '{}',
    description   text NOT NULL DEFAULT '',
    muscle_groups text[] NOT NULL DEFAULT '{}'
                  CHECK (muscle_groups <@ ARRAY['chest', 'back', 'shoulders', 'biceps', 'triceps', 'forearms',
                                                'core', 'glutes', 'quads', 'hamstrings', 'calves', 'cardio']),
    description_tsv tsvector GENERATED ALWAYS AS (to_tsvector('simple', description)) STORED
);
CREATE UNIQUE INDEX exercise_owner_name ON exercise (owner_sub, lower(name));
CREATE INDEX exercise_description_tsv ON exercise USING gin (description_tsv);

-- An exercise requires ALL of its equipment; none means bodyweight.
CREATE TABLE exercise_equipment (
    exercise_id  bigint NOT NULL REFERENCES exercise (id) ON DELETE CASCADE,
    equipment_id bigint NOT NULL REFERENCES equipment (id) ON DELETE CASCADE,
    PRIMARY KEY (exercise_id, equipment_id)
);

CREATE TABLE workout (
    id            bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    owner_sub     text NOT NULL,
    gym_id        bigint NOT NULL REFERENCES gym (id),
    started_at    timestamptz NOT NULL,
    finished_at   timestamptz,
    finish_reason text CHECK (finish_reason IN ('manual', 'stale', 'superseded')),
    notes         text NOT NULL DEFAULT '',
    created_at    timestamptz NOT NULL DEFAULT now(),
    notes_tsv     tsvector GENERATED ALWAYS AS (to_tsvector('simple', notes)) STORED,
    CHECK ((finished_at IS NULL) = (finish_reason IS NULL)),
    CHECK (finished_at IS NULL OR finished_at >= started_at)
);
CREATE UNIQUE INDEX workout_one_open_per_owner ON workout (owner_sub) WHERE finished_at IS NULL;
CREATE INDEX workout_owner_started ON workout (owner_sub, started_at);
CREATE INDEX workout_notes_tsv ON workout USING gin (notes_tsv);

-- performed_at = when the set happened; logged_at = when the server received it.
-- Set numbers are derived (row_number over performed_at), never stored.
CREATE TABLE workout_set (
    id           bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workout_id   bigint NOT NULL REFERENCES workout (id) ON DELETE CASCADE,
    exercise_id  bigint NOT NULL REFERENCES exercise (id),
    performed_at timestamptz NOT NULL,
    reps         integer CHECK (reps >= 0),
    weight_kg    double precision CHECK (weight_kg >= 0),
    duration_s   integer CHECK (duration_s >= 0),
    notes        text NOT NULL DEFAULT '',
    logged_at    timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    notes_tsv    tsvector GENERATED ALWAYS AS (to_tsvector('simple', notes)) STORED
);
CREATE INDEX workout_set_order ON workout_set (workout_id, exercise_id, performed_at);
CREATE INDEX workout_set_exercise ON workout_set (exercise_id, performed_at);
CREATE INDEX workout_set_notes_tsv ON workout_set USING gin (notes_tsv);
