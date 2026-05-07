fn main() {
    let _ = |mut ctx: djogi::DjogiContext| async move {
        let _ = ctx.raw_ddl("CREATE TABLE example (id BIGINT)").await;
    };
}
