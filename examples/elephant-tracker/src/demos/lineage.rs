//! `lineage` demo — recursive-CTE escape hatch.
//!
//! Djogi does not ship a tree-query API. When you need ancestor/descendant
//! traversal, you drop to raw SQL via the `tokio_postgres::Client`
//! escape hatch. This demo walks the matriarchal lineage starting from a
//! named matriarch and prints the descendant tree.
//!
//! Why this is in the example: an honest framework demo should show
//! the escape hatch, not pretend everything has a typed builder. Trees
//! are a category where dropping to SQL is the right move and the
//! framework should make that easy.

use anyhow::Result;
use djogi::prelude::*;

pub async fn run(ctx: &DjogiContext, matriarch_name: &str) -> Result<()> {
    // Sketch — wired against real APIs once cluster PRs land.
    //
    //     const SQL: &str = r"
    //         WITH RECURSIVE descendants AS (
    //             SELECT id, name, parent_id, 0 AS depth
    //             FROM elephants WHERE name = $1
    //             UNION ALL
    //             SELECT e.id, e.name, e.parent_id, d.depth + 1
    //             FROM elephants e JOIN descendants d ON e.parent_id = d.id
    //         )
    //         SELECT id, name, parent_id, depth FROM descendants ORDER BY depth, name
    //     ";
    //     let client = ctx.client().await?;
    //     let rows = client.query(SQL, &[&matriarch_name]).await?;
    //     for row in rows {
    //         let depth: i32 = row.get("depth");
    //         let name: String = row.get("name");
    //         println!("{}{}", "  ".repeat(depth as usize), name);
    //     }
    let _ = (ctx, matriarch_name);
    todo!("wire recursive CTE")
}
