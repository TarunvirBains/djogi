-- Five countries — the geographic scope of the example.
-- Loaded by `cargo run -- seed` before the programmatic herds + sightings seed runs.

INSERT INTO countries (iso_alpha3, name) VALUES
    ('KEN', 'Kenya'),
    ('TZA', 'Tanzania'),
    ('UGA', 'Uganda'),
    ('BWA', 'Botswana'),
    ('ZWE', 'Zimbabwe')
ON CONFLICT (iso_alpha3) DO NOTHING;
