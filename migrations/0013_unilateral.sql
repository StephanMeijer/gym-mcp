-- unilateral: reps are per side and the set counts once per side.
-- load_basis: what weight_kg means; NULL = the default convention (per implement, barbell total, stack setting).
ALTER TABLE workout_set
    ADD COLUMN unilateral boolean NOT NULL DEFAULT false,
    ADD COLUMN load_basis text CHECK (load_basis IN ('per_implement', 'per_side', 'total'));
