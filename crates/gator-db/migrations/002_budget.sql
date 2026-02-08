-- Add optional token budget to plans.
-- NULL means unlimited; counts total input+output tokens.
ALTER TABLE plans ADD COLUMN token_budget BIGINT;
