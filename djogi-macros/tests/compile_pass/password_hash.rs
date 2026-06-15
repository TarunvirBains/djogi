//! Compile-pass: `PasswordHash` is usable as a field type in user models
//! and compiles against the public `djogi::auth::PasswordHash` surface.

use djogi::auth::PasswordHash;

#[allow(dead_code)]
pub struct MyUser {
 pub password_hash: PasswordHash,
}

fn main() {
 let _ = std::mem::size_of::<MyUser>();
}
