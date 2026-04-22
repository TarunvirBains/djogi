//! Compile-pass: a custom `DjogiAuth` implementation compiles against the
//! public trait surface exposed from the `djogi` crate.
//!
//! This fixture exists to detect breakage of the pluggable-provider contract.
//! If a future change to `DjogiAuth` accidentally adds a method with no
//! default impl, removes object safety, or shifts a bound, this file stops
//! compiling and the compile_pass harness fails.

use djogi::auth::{AuthContext, AuthError, DjogiAuth};
use std::future::Future;
use std::pin::Pin;

pub struct MyProvider;

impl DjogiAuth for MyProvider {
    fn authenticate<'a>(
        &'a self,
        token: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<AuthContext, AuthError>> + Send + 'a>> {
        let _ = token;
        Box::pin(async { Err(AuthError::InvalidToken) })
    }
}

fn main() {
    // Object-safe: can be used behind a trait object.
    let _: std::sync::Arc<dyn DjogiAuth> = std::sync::Arc::new(MyProvider);
}
