-- unilateral: reps are per side and the set counts once per side.
-- load_basis: what weight_kg means on a unilateral set; NULL = the default convention (the load one side moved).
-- Only 'total' changes a number; per_implement and per_side state the default explicitly.
ALTER TABLE workout_set
    ADD COLUMN unilateral boolean NOT NULL DEFAULT false,
    ADD COLUMN load_basis text CHECK (load_basis IN ('per_implement', 'per_side', 'total')),
    ADD CHECK (load_basis IS NULL OR unilateral);
