> [Back to README](../../ReadMe.MD) | [Gap Analysis](../spec/orm-gap-analysis.md)

# Django 6.0 contrib.postgres — Full Catalog for Djogi

*Since Djogi is Postgres-only, everything here is first-class, not optional contrib.*

## Key Findings for Djogi

### Must Have (First-Class)

1. **ArrayField** → `Array<T>` — native Postgres arrays with contains/overlap/len operators, index/slice transforms
2. **Full-text search** — SearchVector, SearchQuery, SearchRank, SearchHeadline, trigram similarity. This is a killer feature Django buries in contrib.
3. **Range fields** → `Range<T>` — int4range, daterange, tstzrange, etc. with all 8 range operators
4. **GIN/GiST/BRIN indexes** — with tuning parameters (fastupdate, pages_per_range, etc.)
5. **ExclusionConstraint** — no equivalent in other DBs. Essential for scheduling/booking.
6. **ArrayAgg / JSONBAgg** — build arrays/JSON in a single query
7. **Concurrent index operations** — CREATE INDEX CONCURRENTLY for zero-downtime migrations
8. **NOT VALID + VALIDATE CONSTRAINT** — two-phase constraint addition for production deployments
9. **ArraySubquery** — `ARRAY(SELECT ...)` pattern

### Valuable Ergonomics

10. **BoolAnd/BoolOr** — batch validation, permission checks
11. **Statistical aggregates** (Corr, RegrSlope, etc.) — analytics-heavy apps
12. **OpClass** — operator class binding for indexes
13. **Unaccent** — bilateral transform for accent-insensitive search
14. **CreateExtension** — declarative extension management in migrations

### Skip or Simplify

- **HStoreField** — largely superseded by JSONB
- **CIText fields** — deprecated even in Django 6.0, use collations
- **Backend dispatch machinery** — Djogi is Postgres-only, no need
- **Extension shorthand classes** — just use `CreateExtension`

*Full detailed catalog available in the research agent output.*
