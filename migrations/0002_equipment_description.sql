-- What a kind of equipment is, the same at every gym ("45° plate-loaded leg press").
-- Gym-specific details stay in gym_equipment.notes.
ALTER TABLE equipment
    ADD COLUMN description text NOT NULL DEFAULT '',
    ADD COLUMN description_tsv tsvector GENERATED ALWAYS AS (to_tsvector('simple', description)) STORED;
CREATE INDEX equipment_description_tsv ON equipment USING gin (description_tsv);
