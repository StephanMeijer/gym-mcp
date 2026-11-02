-- Body height ("length"), stored in cm.
ALTER TABLE measurement
    DROP CONSTRAINT measurement_kind_check,
    DROP CONSTRAINT measurement_check,
    ADD CONSTRAINT measurement_kind_check CHECK (kind IN ('body_weight', 'height')),
    ADD CONSTRAINT measurement_check CHECK ((kind, unit) IN (('body_weight', 'kg'), ('height', 'cm')));
