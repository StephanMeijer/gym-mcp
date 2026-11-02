-- Equipment stops being an entity: gyms and exercises get a free-text `equipment` field,
-- and Claude judges what's possible by reading them. Existing structured data is folded
-- into the new text fields first, so nothing recorded is lost.

ALTER TABLE gym ADD COLUMN equipment text NOT NULL DEFAULT '';
ALTER TABLE exercise ADD COLUMN equipment text NOT NULL DEFAULT '';

-- Gym: one line per item that's present ("leg press (machine): 45° sled; max 300 kg; two, upstairs"),
-- then one "Not available:" line, and a note if the inventory was marked complete.
UPDATE gym g SET equipment = concat_ws(E'\n',
    (SELECT string_agg(
                concat_ws('; ',
                    eq.name || ' (' || replace(eq.category, '_', ' ') || ')' || CASE WHEN eq.description <> '' THEN ': ' || eq.description ELSE '' END,
                    CASE WHEN ge.quantity IS NOT NULL THEN ge.quantity || '×' END,
                    CASE WHEN ge.available_weights_kg IS NOT NULL
                         THEN array_to_string(ge.available_weights_kg, ', ') || ' kg' END,
                    CASE WHEN ge.max_load_kg IS NOT NULL THEN 'max ' || ge.max_load_kg || ' kg' END,
                    NULLIF(ge.notes, '')),
                E'\n' ORDER BY eq.category, eq.name)
     FROM gym_equipment ge JOIN equipment eq ON eq.id = ge.equipment_id
     WHERE ge.gym_id = g.id AND ge.status = 'present'),
    (SELECT 'Not available: ' || string_agg(eq.name, ', ' ORDER BY eq.name)
     FROM gym_equipment ge JOIN equipment eq ON eq.id = ge.equipment_id
     WHERE ge.gym_id = g.id AND ge.status = 'absent'),
    CASE WHEN g.inventory_complete THEN 'This list is complete.' END);

UPDATE exercise e SET equipment = coalesce(
    (SELECT string_agg(eq.name, ' + ' ORDER BY eq.name)
     FROM exercise_equipment ee JOIN equipment eq ON eq.id = ee.equipment_id
     WHERE ee.exercise_id = e.id), '');

DROP TABLE exercise_equipment, gym_equipment, equipment;
ALTER TABLE gym DROP COLUMN inventory_complete;

ALTER TABLE gym ADD COLUMN equipment_tsv tsvector GENERATED ALWAYS AS (to_tsvector('simple', equipment)) STORED;
ALTER TABLE exercise ADD COLUMN equipment_tsv tsvector GENERATED ALWAYS AS (to_tsvector('simple', equipment)) STORED;
CREATE INDEX gym_equipment_tsv ON gym USING gin (equipment_tsv);
CREATE INDEX exercise_equipment_tsv ON exercise USING gin (equipment_tsv);
