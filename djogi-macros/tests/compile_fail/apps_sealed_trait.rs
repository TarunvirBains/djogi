// `djogi::App` is sealed. Manual impls outside
// the `djogi::apps!` macro are rejected by the `Sealed` supertrait.
use djogi::App;

pub struct Foo;

impl App for Foo {
 const LABEL: &'static str = "foo";
 const DATABASE: &'static str = "main";
 const DESCRIPTOR: djogi::AppDescriptor = djogi::AppDescriptor {
  label: "foo",
  database: "main",
  renamed_from: None,
  tombstone: false,
 };
}

fn main() {}
