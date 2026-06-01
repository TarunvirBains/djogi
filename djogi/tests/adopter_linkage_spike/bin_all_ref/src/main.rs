//! Bin B: References BOTH `Elephant::descriptor` AND `Herd::descriptor` from tracker.
//! Also depends on billing but references NOTHING from it.

use djogi::descriptor::ModelDescriptor;

fn main() {
    let _elephant_desc = <tracker::Elephant as djogi::model::Model>::descriptor;
    let _herd_desc = <tracker::Herd as djogi::model::Model>::descriptor;

    // Do NOT reference Invoice from billing at all.

    std::hint::black_box(&_elephant_desc);
    std::hint::black_box(&_herd_desc);

    let models: Vec<&ModelDescriptor> = inventory::iter::<ModelDescriptor>().collect();
    println!("Total descriptors: {}", models.len());

    let mut tracker_models: Vec<_> = models.iter()
        .filter(|m| m.table_name.starts_with("tracker_"))
        .collect();
    tracker_models.sort_by_key(|m| m.table_name);

    let mut billing_models: Vec<_> = models.iter()
        .filter(|m| m.table_name.starts_with("billing_"))
        .collect();
    billing_models.sort_by_key(|m| m.table_name);

    println!("\ntracker crate models (table_name starts with 'tracker_'):");
    if tracker_models.is_empty() {
        println!("  (none)");
    } else {
        for m in &tracker_models {
            println!("  {} -> {}", m.type_name, m.table_name);
        }
    }
    println!("billing crate models (table_name starts with 'billing_'):");
    if billing_models.is_empty() {
        println!("  (none)");
    } else {
        for m in &billing_models {
            println!("  {} -> {}", m.type_name, m.table_name);
        }
    }

    println!("\nAll descriptors:");
    let mut all_sorted: Vec<_> = models.iter().collect();
    all_sorted.sort_by_key(|m| (m.type_name, m.table_name));
    for m in &all_sorted {
        println!("  {} -> {}", m.type_name, m.table_name);
    }
}
