-- Body weight, height, waist and resting heart rate are no longer tracked; their rows go.
DELETE FROM measurement WHERE kind IN ('body_weight', 'body_height', 'waist_circumference', 'resting_heart_rate');
ALTER TABLE measurement
    DROP CONSTRAINT measurement_kind_check,
    DROP CONSTRAINT measurement_check,
    ADD CONSTRAINT measurement_kind_check CHECK (kind IN ('body_fat', 'blood_pressure_systolic', 'blood_pressure_diastolic')),
    ADD CONSTRAINT measurement_check CHECK ((kind, unit) IN (('body_fat', '%'),
        ('blood_pressure_systolic', 'mmHg'), ('blood_pressure_diastolic', 'mmHg')));
