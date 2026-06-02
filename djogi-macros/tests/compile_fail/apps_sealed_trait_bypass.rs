// the old `djogi::apps::sealed::Sealed`
// bypass path is no longer public.

pub struct FakeApp;

impl djogi::apps::sealed::Sealed for FakeApp {}

fn main() {}
