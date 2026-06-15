//! Bin C: References NOTHING from billing.
//! Depends on billing crate but does not use any of its types.

use djogi::descriptor::ModelDescriptor;

fn main() {
 // Do NOT reference ANY model types from billing at all.

 let models: Vec<&ModelDescriptor> = inventory::iter::<ModelDescriptor>().collect();
 println!("Total descriptors: {}", models.len());

 let mut billing_models: Vec<_> = models.iter()
 .filter(|m| m.table_name.starts_with("billing_"))
 .collect();
 billing_models.sort_by_key(|m| m.table_name);

 println!("\nbilling crate models (table_name starts with 'billing_'):");
 if billing_models.is_empty() {
  println!(" (none — billing crate was dropped by linker)");
 } else {
  for m in &billing_models {
   println!(" {} -> {}", m.type_name, m.table_name);
  }
 }

 println!("\nAll descriptors:");
 let mut all_sorted: Vec<_> = models.iter().collect();
 all_sorted.sort_by_key(|m| (m.type_name, m.table_name));
 if all_sorted.is_empty() {
  println!(" (none)");
 } else {
  for m in &all_sorted {
   println!(" {} -> {}", m.type_name, m.table_name);
  }
 }
}
