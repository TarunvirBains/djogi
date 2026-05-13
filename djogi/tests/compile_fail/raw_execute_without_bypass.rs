fn main() {
    let _ = |mut ctx: djogi::DjogiContext| async move {
        let _ = ctx.raw_execute("SELECT 1", &[]).await;
    };
}
