// .7 — Canonical-sequence integration test.
//
// Pins the full six-hook ordering through a single create → save →
// delete pipeline:
//
//   before_create → INSERT → after_create
//   before_save  → UPDATE → after_save
//   before_delete → DELETE → after_delete
//
// .4 / .5 / .6 each pin one CRUD terminal in isolation. This
// file proves the three terminals cooperate — that no rebinding,
// shadowing, or branch-folding regression in any one of them
// disturbs the relative ordering when all six hooks fire across one
// row's lifecycle.
//
// §D3 lines 118-129 fix the canonical sequence as
// `before_<verb> → SQL → outbox → after_<verb> → on_commit drain`
// for each of {create, save, delete}. The vec assertion below pins
// the verb-level interleaving without depending on the inter-step
// outbox / on_commit framing (those are pinned by .4–.6
// individually).
//
// # Why `OnceLock<Mutex<Vec<&'static str>>>`
//
// Per the spec (.md lines 549–599) and the
// `feedback_log_codex_findings.md` design, the recorder is
// `static ORDER: OnceLock<Mutex<Vec<&'static str>>>`. `OnceLock` gives
// us safe lazy init, `Mutex` makes the push interior-mutable across
// the six `&self` / `&mut self` hook bodies, and the contained
// `Vec<&'static str>` is the simplest possible append-only log.
//
// `static mut` would be UB; `tokio::task_local!` (used by .5 / .6)
// is overkill for a single-test file with no shared model. If a
// future test in this file declares a second model, the recorder
// must be scoped via separate `OnceLock`s or via `task_local!` to
// avoid cross-test interference; see the file-header note in
// `hooks_save.rs` for the established convention.

use djogi::prelude::*;
use std::sync::{Mutex, OnceLock};

static ORDER: OnceLock<Mutex<Vec<&'static str>>> = OnceLock::new();

fn record(s: &'static str) {
    ORDER
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap()
        .push(s);
}

#[model(table = "hooks_canonical_probes", pk = HeerId, hooks)]
#[derive(Debug, Clone)]
pub struct Probe {
    pub value: i32,
}

impl djogi::hooks::ModelHooks for Probe {
    async fn before_create(
        &mut self,
        _ctx: &mut djogi::DjogiContext,
    ) -> Result<(), djogi::DjogiError> {
        record("before_create");
        Ok(())
    }

    async fn after_create(&self, _ctx: &mut djogi::DjogiContext) -> Result<(), djogi::DjogiError> {
        record("after_create");
        Ok(())
    }

    async fn before_save(
        &mut self,
        _ctx: &mut djogi::DjogiContext,
    ) -> Result<(), djogi::DjogiError> {
        record("before_save");
        Ok(())
    }

    async fn after_save(&self, _ctx: &mut djogi::DjogiContext) -> Result<(), djogi::DjogiError> {
        record("after_save");
        Ok(())
    }

    async fn before_delete(
        &mut self,
        _ctx: &mut djogi::DjogiContext,
    ) -> Result<(), djogi::DjogiError> {
        record("before_delete");
        Ok(())
    }

    async fn after_delete(&self, _ctx: &mut djogi::DjogiContext) -> Result<(), djogi::DjogiError> {
        record("after_delete");
        Ok(())
    }
}

#[djogi::djogi_test(sync_models = [Probe])]
async fn canonical_sequence_create_save_delete(mut ctx: djogi::DjogiContext) {
    // Defensive reset — `OnceLock` is process-global, so if cargo
    // ever runs this test more than once in a single process (e.g.
    // under a future hot-reload harness) we want a clean log each
    // time. `#[djogi_test]` already gives us a fresh DB per test, so
    // this is the only piece of state that survives across runs.
    {
        let cell = ORDER.get_or_init(|| Mutex::new(Vec::new()));
        cell.lock().unwrap().clear();
    }

    let mut p = Probe::create(
        &mut ctx,
        Probe {
            value: 1,
            ..Default::default()
        },
    )
    .await
    .expect("create should succeed and run before_create + after_create");

    p.value = 2;
    p.save(&mut ctx)
        .await
        .expect("save should succeed and run before_save + after_save");

    p.delete(&mut ctx)
        .await
        .expect("delete should succeed and run before_delete + after_delete");

    let order = ORDER
        .get()
        .expect("ORDER must be initialised by the first hook call")
        .lock()
        .unwrap();
    assert_eq!(
        &*order,
        &[
            "before_create",
            "after_create",
            "before_save",
            "after_save",
            "before_delete",
            "after_delete",
        ],
        "canonical hook order must hold \
     across the full create → save → delete lifecycle",
    );
}
