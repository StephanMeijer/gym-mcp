-- Waist circumference, body fat %, resting heart rate and blood pressure (systolic and diastolic
-- as two kinds, so every row stays one value).
ALTER TABLE measurement
    DROP CONSTRAINT measurement_kind_check,
    DROP CONSTRAINT measurement_check,
    ADD CONSTRAINT measurement_kind_check CHECK (kind IN ('body_weight', 'body_height', 'waist_circumference',
        'body_fat', 'resting_heart_rate', 'blood_pressure_systolic', 'blood_pressure_diastolic')),
    ADD CONSTRAINT measurement_check CHECK ((kind, unit) IN (('body_weight', 'kg'), ('body_height', 'cm'),
        ('waist_circumference', 'cm'), ('body_fat', '%'), ('resting_heart_rate', 'bpm'),
        ('blood_pressure_systolic', 'mmHg'), ('blood_pressure_diastolic', 'mmHg'))),
    ADD CONSTRAINT measurement_body_fat_range CHECK (kind <> 'body_fat' OR value < 100);
