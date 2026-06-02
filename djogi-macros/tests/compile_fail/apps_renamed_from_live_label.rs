// `renamed_from = "vehicles"` while `Vehicles`
// is still declared live is rejected. Rename retires the old
// label; the old declaration must go away in the same commit.

djogi::apps! {
    #[app(database = "main")]
    pub struct Vehicles;

    #[app(database = "main", renamed_from = "vehicles")]
    pub struct NewVehicles;
}

fn main() {}
