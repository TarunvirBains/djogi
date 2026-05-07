//! Programmatic + SQL seed.
//!
//! Layered: `seeds/countries.sql` (idempotent, hand-written) loads the
//! five reference rows; `seed_herds_and_elephants_and_sightings` runs
//! the programmatic batch under one `atomic()` scope so a failure
//! halfway through does not leave the example in a partial state.
//!
//! The numbers below match what the demos expect:
//! - 4 herds, each spanning 2 countries (one in dry season, one in
//!   wet season — picked so the cross-border-herds demo finds at
//!   least one wet-season cross-border herd).
//! - ~30 elephants per herd: 1 matriarch, 4-6 daughters, 1-2 grandkids
//!   per daughter.
//! - 1 researcher per herd, all under one `org_id` so the
//!   `researchers` table is multi-tenant-shaped without exercising
//!   actual tenancy in the example.
//! - ~50 sightings per herd, distributed in three loose clusters per
//!   herd so the `cluster-sightings` demo finds real density hotspots.

use anyhow::{Context, Result};
use djogi::__bypass::{RawAccessExt as _, RawPoolAccessExt as _};
use djogi::pg::pool::DjogiPool;
use djogi::prelude::*;
use djogi::transaction::atomic;
use time::OffsetDateTime;

use crate::models::{
    Country, Elephant, ElephantAncestry, ElephantTags, Herd, HerdRange, Researcher, Sighting,
};

const COUNTRIES_SQL: &str = include_str!("../seeds/countries.sql");

const ORG_ID: i64 = 1;

/// Per-herd seed data. The matriarch + 3 unrelated bulls + 5
/// daughters + each daughter's 1-2 grandkids gives ~16 elephants per
/// herd through the programmatic generator below; the helper rounds
/// to 30 with a tail of `Calf-NN` joiners whose mother / father may
/// or may not be known. Mother / father / sex are assigned via
/// deterministic LCG so successive runs against a fresh DB produce
/// identical population graphs.
struct HerdSeed {
    name: &'static str,
    matriarch: &'static str,
    daughters: &'static [&'static str],
    grandkids: &'static [&'static [&'static str]],
    /// `(country_iso3, season)` rows the herd visits.
    ranges: &'static [(&'static str, &'static str)],
    /// Approximate centre lat/lon for the herd's home water source.
    /// Sightings are jittered around this point.
    center_lat: f64,
    center_lon: f64,
    /// Population estimate at last census.
    estimated_population: i32,
    /// Researcher's name + email + notes.
    researcher: (&'static str, &'static str, &'static str),
}

const HERDS: &[HerdSeed] = &[
    HerdSeed {
        name: "Amboseli-A",
        matriarch: "Wema",
        daughters: &["Amani", "Bahati", "Chui", "Dada", "Ebony"],
        grandkids: &[
            &["Fadhili", "Gloria"],
            &["Heshima"],
            &["Imani", "Jasiri"],
            &["Kifaru"],
            &["Lulu", "Maua"],
        ],
        ranges: &[
            ("KEN", "dry"),
            ("TZA", "dry"),
            ("KEN", "wet"),
            ("TZA", "wet"),
        ],
        center_lat: -2.65,
        center_lon: 36.97,
        estimated_population: 78,
        researcher: (
            "Aisha Otieno",
            "aisha@example.org",
            "Amboseli matriarch tracker; herd at watering hole; tusks intact",
        ),
    },
    HerdSeed {
        name: "Maasai-Mara-B",
        matriarch: "Nuru",
        daughters: &["Onyx", "Pumzi", "Rafiki", "Shani", "Tama"],
        grandkids: &[
            &["Uhuru"],
            &["Vita", "Waridi"],
            &["Xena"],
            &["Yusra", "Zola"],
            &["Asha"],
        ],
        ranges: &[("KEN", "wet"), ("TZA", "dry"), ("UGA", "wet")],
        center_lat: -1.50,
        center_lon: 35.07,
        estimated_population: 102,
        researcher: (
            "Brian Kimani",
            "brian@example.org",
            "Maasai-Mara morning patrol; herd grazing in tall grass; calm temperament",
        ),
    },
    HerdSeed {
        name: "Selous-C",
        matriarch: "Bahati",
        daughters: &["Cara", "Dalia", "Ester", "Fanaka"],
        grandkids: &[
            &["Gemma", "Halua"],
            &["Inka"],
            &["Jami", "Kente"],
            &["Layla"],
        ],
        ranges: &[("TZA", "dry"), ("UGA", "wet")],
        center_lat: -8.50,
        center_lon: 38.00,
        estimated_population: 55,
        researcher: (
            "Catherine Mtui",
            "catherine@example.org",
            "Selous reserve census; herd at salt lick; muddy feet noted",
        ),
    },
    HerdSeed {
        name: "Hwange-D",
        matriarch: "Mara",
        daughters: &["Nala", "Ola", "Penda", "Quincy", "Rumi", "Saba"],
        grandkids: &[
            &["Tane"],
            &["Uka", "Vesna"],
            &["Wisi"],
            &["Xola"],
            &["Yara", "Zula"],
            &["Akili"],
        ],
        ranges: &[("ZWE", "dry"), ("BWA", "wet"), ("BWA", "dry")],
        center_lat: -18.66,
        center_lon: 26.96,
        estimated_population: 134,
        researcher: (
            "David Ncube",
            "david@example.org",
            "Hwange waterhole survey; herd active at dusk; one bull observed",
        ),
    },
];

/// Run the full seed.
pub async fn run(ctx: &mut DjogiContext) -> Result<()> {
    tracing::info!("loading seeds/countries.sql");
    ctx.raw_ddl(COUNTRIES_SQL)
        .await
        .context("apply seeds/countries.sql")?;

    let pool = ctx
        .raw_pool()
        .ok_or_else(|| anyhow::anyhow!("ctx must be pool-backed for seed"))?
        .clone();

    tracing::info!("seeding herds, ranges, elephants, researchers, sightings");
    seed_programmatic(&pool).await?;

    tracing::info!("seed complete");
    Ok(())
}

/// Programmatic seed wrapped in a single `atomic()` scope.
async fn seed_programmatic(pool: &DjogiPool) -> Result<()> {
    atomic(pool, |ctx| {
        Box::pin(async move {
            // Pull all five countries up front — we need their PKs to
            // build HerdRange rows. `Country::objects().fetch_all`
            // would also work; raw_query keeps the lookup minimal.
            let countries: Vec<Country> = ctx
                .raw_query("SELECT * FROM countries ORDER BY iso_alpha3", &[])
                .await?;

            for spec in HERDS {
                seed_one_herd(ctx, spec, &countries).await?;
            }

            // Materialize the pedigree closure once every elephant +
            // both self-FK edges (`mother_id`, `father_id`) are
            // committed. Walks both edges in a single recursive CTE
            // up to depth 5 (enough to reach the matriarch + bull
            // generation from any tail calf in this seed graph). The
            // mating-pairs demo (T24) consumes the closure for O(1)-
            // lookup-per-pair Wright F computation.
            let report = Elephant::materialize_closure::<ElephantAncestry>(
                ctx,
                djogi::query::MaterializeClosureOptions::default().with_max_depth(5),
            )
            .await?;
            tracing::info!(
                rows_written = report.rows_written,
                sources_visited = report.sources_visited,
                "materialized ElephantAncestry closure"
            );

            Ok::<_, DjogiError>(())
        })
    })
    .await
    .map_err(|e| anyhow::anyhow!("seed transaction failed: {e}"))?;
    Ok(())
}

async fn seed_one_herd(
    ctx: &mut DjogiContext,
    spec: &HerdSeed,
    countries: &[Country],
) -> Result<(), DjogiError> {
    // 1) Herd row.
    let herd = Herd::create(
        ctx,
        Herd {
            name: spec.name.to_string(),
            estimated_population: spec.estimated_population,
            ..Default::default()
        },
    )
    .await?;

    // 2) HerdRange rows — one per `(country, season)` pair.
    for (iso3, season) in spec.ranges {
        let country = countries
            .iter()
            .find(|c| c.iso_alpha3.as_str() == *iso3)
            .ok_or_else(|| {
                DjogiError::Db(djogi::DbError::other(format!(
                    "country {iso3} not seeded by countries.sql"
                )))
            })?;
        let _ = HerdRange::create(
            ctx,
            HerdRange {
                id: <djogi::HeerId as djogi::PrimaryKey>::sentinel(),
                created_at: djogi::DateTime::UNIX_EPOCH,
                updated_at: djogi::DateTime::UNIX_EPOCH,
                herd_id: ForeignKey::new(herd.id),
                country_id: ForeignKey::new(country.id),
                season: (*season).to_string(),
            },
        )
        .await?;
    }

    // 3) Researcher row.
    let researcher = Researcher::create(
        ctx,
        Researcher {
            org_id: ORG_ID,
            name: Tracked::new(spec.researcher.0.to_string()),
            email: spec.researcher.1.to_string(),
            notes: spec.researcher.2.to_string(),
            ..Default::default()
        },
    )
    .await?;

    // 4) Elephants — matriarch -> bull pool -> daughters -> grandkids -> tail.
    //
    // The bull pool is created first so daughters and grandkids can
    // draw fathers from it. Bulls are unrelated males with `sex = "m"`,
    // born 2008-2010, paternity-unknown themselves (fathers are
    // peripheral in elephant society — observed but not pedigree-
    // tracked). The pool size of 3 per herd plus deterministic LCG
    // selection produces realistic father-coverage rates without a
    // combinatorial seed-data explosion.
    //
    // Wild-herd realism (population-wide). Mothers are tightly
    // observed (calves nurse, herds are matrilineal); fathers are
    // inferred from herd-overlap observations but rarely confirmed.
    // Realized coverage on the deterministic 120-elephant seed:
    // **63.3% known mother, 41.7% known father** (4 herds × 30
    // elephants). Bumping these closer to the v3 plan's aspirational
    // `~70% / ~40%` target would require either expanding the bull
    // pool or skewing the per-elephant probability dials further;
    // the current dials produce a graph rich enough for Wright F
    // values across a meaningful slice of the population without
    // over-saturating known parentage past biological realism.
    let mut rng_lineage = Lcg::new(spec.name);
    let matriarch = Elephant::create(
        ctx,
        elephant_for_insert(spec.matriarch, herd.id, None, None, Some(2010), Some("f")),
    )
    .await?;

    // Bull pool — 3 unrelated males per herd, born 2008-2010 so they
    // are sexually mature when daughters bear grandkids ~2022.
    let mut bull_ids: Vec<djogi::HeerId> = Vec::with_capacity(3);
    for i in 0..3 {
        let bull_name = format!("{}-Bull-{}", spec.name, i + 1);
        let bull = Elephant::create(
            ctx,
            elephant_for_insert(
                &bull_name,
                herd.id,
                None,
                None,
                Some(2008 + i as i16),
                Some("m"),
            ),
        )
        .await?;
        bull_ids.push(bull.id);
    }
    let pick_father = |rng: &mut Lcg, prob_pct: u32| -> Option<djogi::HeerId> {
        if rng.next_u32() % 100 < prob_pct {
            Some(bull_ids[(rng.next_u32() as usize) % bull_ids.len()])
        } else {
            None
        }
    };

    let mut daughter_ids: Vec<djogi::HeerId> = Vec::with_capacity(spec.daughters.len());
    for d in spec.daughters {
        // Daughters: 80% known father (one of the herd's bulls).
        let father = pick_father(&mut rng_lineage, 80);
        let row = Elephant::create(
            ctx,
            elephant_for_insert(
                d,
                herd.id,
                Some(matriarch.id),
                father,
                Some(2016),
                Some("f"),
            ),
        )
        .await?;
        daughter_ids.push(row.id);
    }

    for (i, kids) in spec.grandkids.iter().enumerate() {
        let parent = daughter_ids[i];
        for k in *kids {
            // Grandkids: 70% known father; sex 50/50 m/f.
            let father = pick_father(&mut rng_lineage, 70);
            let sex = if rng_lineage.next_u32().is_multiple_of(2) {
                "f"
            } else {
                "m"
            };
            let _ = Elephant::create(
                ctx,
                elephant_for_insert(k, herd.id, Some(parent), father, Some(2022), Some(sex)),
            )
            .await?;
        }
    }

    // Round to 30 per herd with a deterministic tail of unrelated
    // elephants — joiners or recent additions whose lineage is
    // partially recorded. Mother known 60% of the time, father 25%,
    // sex 50/50 m/f. The tail's tilt toward known-mother / unknown-
    // father pulls population-wide coverage toward the realistic
    // ~70%-mother / ~40%-father target documented in the v3 plan.
    let total_so_far = 1 // matriarch
        + bull_ids.len()
        + spec.daughters.len()
        + spec.grandkids.iter().map(|g| g.len()).sum::<usize>();
    let known_females = std::iter::once(matriarch.id)
        .chain(daughter_ids.iter().copied())
        .collect::<Vec<_>>();
    for i in total_so_far..30 {
        let name = format!("Calf-{i:02}");
        let mother = if rng_lineage.next_u32() % 100 < 60 {
            Some(known_females[(rng_lineage.next_u32() as usize) % known_females.len()])
        } else {
            None
        };
        let father = pick_father(&mut rng_lineage, 25);
        let sex = if rng_lineage.next_u32().is_multiple_of(2) {
            "f"
        } else {
            "m"
        };
        let _ = Elephant::create(
            ctx,
            elephant_for_insert(&name, herd.id, mother, father, Some(2020), Some(sex)),
        )
        .await?;
    }

    // 5) Sightings — 50 per herd, three loose clusters around the herd
    // centre. Pseudo-random jitter via a tiny LCG so the placement is
    // deterministic across runs.
    let mut rng = Lcg::new(spec.name);
    let cluster_offsets = [(0.0, 0.0), (0.05, 0.04), (-0.04, 0.03)];
    let now = OffsetDateTime::now_utc();
    for s in 0..50 {
        let cluster = cluster_offsets[s % cluster_offsets.len()];
        let lat = spec.center_lat + cluster.0 + rng.jitter(0.05);
        let lon = spec.center_lon + cluster.1 + rng.jitter(0.05);
        let days_back = (rng.next_u32() % 90) as i64;
        let observed_at = now - time::Duration::days(days_back);
        let notes = sighting_note(s);
        let _ = Sighting::create(
            ctx,
            Sighting {
                id: <djogi::HeerId as djogi::PrimaryKey>::sentinel(),
                created_at: djogi::DateTime::UNIX_EPOCH,
                updated_at: djogi::DateTime::UNIX_EPOCH,
                elephant_id: ForeignKey::new(matriarch.id),
                herd_id: ForeignKey::new(herd.id),
                observed_by_id: ForeignKey::new(researcher.id),
                location: GeoPoint::new(lat, lon).map_err(|e| {
                    DjogiError::Db(djogi::DbError::other(format!("GeoPoint::new: {e}")))
                })?,
                observed_at,
                notes: notes.to_string(),
            },
        )
        .await?;
    }

    Ok(())
}

fn elephant_for_insert(
    name: &str,
    herd_id: djogi::HeerId,
    mother_id: Option<djogi::HeerId>,
    father_id: Option<djogi::HeerId>,
    birth_year: Option<i16>,
    sex: Option<&str>,
) -> Elephant {
    let tags = ElephantTags {
        sex: sex.map(|s| s.to_string()),
        ..ElephantTags::default()
    };
    Elephant {
        id: <djogi::HeerId as djogi::PrimaryKey>::sentinel(),
        created_at: djogi::DateTime::UNIX_EPOCH,
        updated_at: djogi::DateTime::UNIX_EPOCH,
        name: Tracked::new(name.to_string()),
        herd_id: ForeignKey::new(herd_id),
        mother_id: mother_id.map(ForeignKey::new),
        father_id: father_id.map(ForeignKey::new),
        estimated_birth_year: birth_year,
        tags: Jsonb::new(tags),
        version: 0,
    }
}

fn sighting_note(seed: usize) -> &'static str {
    const NOTES: &[&str] = &[
        "herd at watering hole; tusks intact, calm",
        "matriarch leading; mid-morning observation",
        "evening crossing; six adults counted",
        "salt lick; juveniles play-fighting",
        "browsing acacia; relaxed posture",
        "river crossing; calves staying close",
        "sunrise patrol; light dust visible",
        "alarmed posture noted; lions nearby",
    ];
    NOTES[seed % NOTES.len()]
}

/// Tiny LCG keyed off the herd name so jitter is deterministic across
/// runs without pulling in a `rand` dependency.
struct Lcg(u64);

impl Lcg {
    fn new(seed: &str) -> Self {
        let mut state: u64 = 0xcbf29ce484222325;
        for byte in seed.as_bytes() {
            state ^= *byte as u64;
            state = state.wrapping_mul(0x100000001b3);
        }
        Lcg(state)
    }

    fn next_u32(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 32) & 0xFFFF_FFFF) as u32
    }

    /// Returns a value in `[-amplitude, amplitude]`.
    fn jitter(&mut self, amplitude: f64) -> f64 {
        let n = self.next_u32() as f64 / u32::MAX as f64; // 0..=1
        (n * 2.0 - 1.0) * amplitude
    }
}
