-- Add optional container_image column for plans using container isolation.
ALTER TABLE plans ADD COLUMN container_image TEXT;
