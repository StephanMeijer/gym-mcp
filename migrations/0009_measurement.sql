-- Body measurements over time. Each kind stores one canonical unit (input in other units is
-- converted), so values of one kind always compare; later kinds add their (kind, unit) pair.
CREATE TABLE measurement (
    id          bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    owner_sub   text NOT NULL,
    kind        text NOT NULL CHECK (kind IN ('body_weight')),
    value       double precision NOT NULL CHECK (value > 0),
    unit        text NOT NULL,
    measured_at timestamptz NOT NULL,
    notes       text NOT NULL DEFAULT '',
    CHECK ((kind, unit) IN (('body_weight', 'kg')))
);
CREATE INDEX measurement_owner_kind_at ON measurement (owner_sub, kind, measured_at);
