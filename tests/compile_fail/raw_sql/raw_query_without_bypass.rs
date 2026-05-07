fn main() {
    let _ = |mut ctx: djogi::DjogiContext| async move {
        let _ = ctx.raw_query::<Never>("SELECT 1", &[]).await;
    };
}

struct Never;
