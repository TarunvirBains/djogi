fn main() {
 let pool = unreachable_djogi_pool();
 let _ = pool.raw_with_client(|_client| Box::pin(async { Ok(()) }));
}

fn unreachable_djogi_pool() -> djogi::pg::pool::DjogiPool {
 unreachable!()
}
