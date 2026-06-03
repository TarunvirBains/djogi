// Live coverage for visage-scoped traversal and
// reverse-FK / M2M boundary enforcement.
//
// # What this test does
//
// 1. Creates two tables with an FK relationship (`emps → depts`).
// 2. Inserts fixtures so one department has a known name and the
//    employees that point at it form a witness set.
// 3. Runs a typed model queryset over the same fixtures to confirm the
//    FK relationship returns the expected rows.
// 4. Exercises the visage-scoped reverse-FK accessor — a
//    `DeptPublic::employees(ctx)` call that returns `Vec<EmpPublic>`
//    end-to-end against the live Postgres database.
// 5. Exercises the visage-scoped M2M accessor — a
//    `PersonPublic::groups(ctx)` call that returns `Vec<GroupPublic>`
//    walking through a junction table.
//
// # Internal shape assertions
//
// The Condition-level and emitted-SQL shape assertions for these APIs live in
// `tests/internal/visage_traversal_shape.rs`. This ordinary live
// target stays on public typed APIs.
//
// # Why the reverse/M2M live tests.
//
//  is exactly the surface where `{Visage}::fetch` DOES work today
// — the visage-scoped method goes model-scoped query → TryFrom
// projection → `Vec<PeerVisage>`. Testing against a live DB pins
// that the conversion cycle survives real row decoding, real PK
// round-trips, and real closure captures.

use djogi::prelude::*;

fn sentinel_id() -> djogi::types::HeerIdDesc {
    <djogi::types::HeerIdDesc as djogi::PrimaryKey>::sentinel()
}

#[model(table = "live_depts")]
#[derive(Debug, Clone)]
pub struct Dept {
    #[field(expose(public))]
    pub name: String,
}

#[model(table = "live_emps", no_default)]
#[derive(Debug, Clone)]
pub struct Emp {
    #[field(expose(public))]
    pub display_name: String,
    #[field(expose(public -> DeptPublic))]
    pub department: ForeignKey<Dept>,
}

fn emp_for_insert(display_name: &str, department: &Dept) -> Emp {
    Emp {
        id: sentinel_id(),
        created_at: time::OffsetDateTime::now_utc(),
        updated_at: time::OffsetDateTime::now_utc(),
        display_name: display_name.to_string(),
        department: ForeignKey::new(department.id),
    }
}

#[djogi::djogi_test(sync_models = [Dept, Emp])]
async fn typed_fk_filter_returns_related_rows(mut ctx: DjogiContext) {
    let engineering = Dept::create(
        &mut ctx,
        Dept {
            name: "Engineering".to_string(),
            ..Default::default()
        },
    )
    .await
    .expect("create Engineering");
    let marketing = Dept::create(
        &mut ctx,
        Dept {
            name: "Marketing".to_string(),
            ..Default::default()
        },
    )
    .await
    .expect("create Marketing");

    Emp::create(&mut ctx, emp_for_insert("Ada", &engineering))
        .await
        .expect("create Ada");
    Emp::create(&mut ctx, emp_for_insert("Grace", &engineering))
        .await
        .expect("create Grace");
    Emp::create(&mut ctx, emp_for_insert("Mia", &marketing))
        .await
        .expect("create Mia");

    let mut employees: Vec<Emp> = Emp::objects()
        .filter(|f| f.department().eq(ForeignKey::new(engineering.id)))
        .fetch_all(&mut ctx)
        .await
        .expect("typed FK filter must succeed");
    employees.sort_by(|a, b| a.display_name.cmp(&b.display_name));
    assert_eq!(employees.len(), 2, "Ada + Grace are in Engineering");
    assert_eq!(employees[0].display_name, "Ada");
    assert_eq!(employees[1].display_name, "Grace");
}

// ---  — reverse-FK visage boundary live coverage ---

#[model(table = "live_v9_depts")]
#[derive(Debug, Clone)]
pub struct RevDept {
    #[field(expose(public))]
    pub name: String,
}

#[model(table = "live_v9_emps", no_default)]
#[derive(Debug, Clone)]
pub struct RevEmp {
    #[field(expose(public))]
    pub display_name: String,
    pub department: ForeignKey<RevDept>,
}

djogi::reverse_one_to_many!(
    RevDept, employees -> RevEmp by department,
    expose(public -> RevEmpPublic)
);

#[djogi::djogi_test(sync_models = [RevDept, RevEmp])]
async fn reverse_fk_visage_accessor_projects_to_peer_visage(mut ctx: DjogiContext) {
    // Create a department + two employees pointing at it through the
    // model CRUD surface so RETURNING id cycles through the
    // heerid_next_desc() default and the PK lifecycle is exercised
    // end-to-end.
    let dept = RevDept::create(
        &mut ctx,
        RevDept {
            id: <djogi::types::HeerIdDesc as djogi::PrimaryKey>::sentinel(),
            created_at: time::OffsetDateTime::now_utc(),
            updated_at: time::OffsetDateTime::now_utc(),
            name: "Engineering".to_string(),
        },
    )
    .await
    .expect("create dept");
    let _ = RevEmp::create(
        &mut ctx,
        RevEmp {
            id: <djogi::types::HeerIdDesc as djogi::PrimaryKey>::sentinel(),
            created_at: time::OffsetDateTime::now_utc(),
            updated_at: time::OffsetDateTime::now_utc(),
            display_name: "Ada".to_string(),
            department: ForeignKey::new(dept.id),
        },
    )
    .await
    .expect("create Ada");
    let _ = RevEmp::create(
        &mut ctx,
        RevEmp {
            id: <djogi::types::HeerIdDesc as djogi::PrimaryKey>::sentinel(),
            created_at: time::OffsetDateTime::now_utc(),
            updated_at: time::OffsetDateTime::now_utc(),
            display_name: "Grace".to_string(),
            department: ForeignKey::new(dept.id),
        },
    )
    .await
    .expect("create Grace");

    // Drive the visage-scoped reverse accessor through the DeptPublic
    // visage. a returns a SELECT-narrowed
    // `VisageQuerySet<RevEmpPublic>`; the caller chains `.fetch_all(ctx)`.
    let dept_public = RevDeptPublic::from(&dept);
    let employees_qs = dept_public.employees();

    let employees: Vec<RevEmpPublic> = employees_qs
        .fetch_all(&mut ctx)
        .await
        .expect("visage reverse accessor must succeed");

    assert_eq!(employees.len(), 2, "both employees roll up");
    let mut names: Vec<&str> = employees.iter().map(|e| e.display_name.as_str()).collect();
    names.sort();
    assert_eq!(names, vec!["Ada", "Grace"]);
}

// ---  — M2M visage boundary live coverage ---

#[model(table = "live_v9_m2m_persons")]
#[derive(Debug, Clone)]
pub struct M2mPerson {
    #[field(expose(public))]
    pub name: String,
}

#[model(table = "live_v9_m2m_groups")]
#[derive(Debug, Clone)]
pub struct M2mGroup {
    #[field(expose(public))]
    pub name: String,
}

#[model(table = "live_v9_m2m_person_groups", through, no_default)]
#[derive(Debug, Clone)]
pub struct M2mPersonGroup {
    pub person_id: ForeignKey<M2mPerson>,
    pub group_id: ForeignKey<M2mGroup>,
    #[field(expose(public))]
    pub role: String,
}

djogi::many_to_many!(
    M2mPerson, M2mGroup,
    through = M2mPersonGroup,
    this_fk = person_id,
    that_fk = group_id,
    relation = "groups",
    expose(public -> M2mGroupPublic)
);

#[djogi::djogi_test(sync_models = [M2mPerson, M2mGroup, M2mPersonGroup])]
async fn m2m_visage_accessor_returns_peer_visage(mut ctx: DjogiContext) {
    let ada = M2mPerson::create(
        &mut ctx,
        M2mPerson {
            id: <djogi::types::HeerIdDesc as djogi::PrimaryKey>::sentinel(),
            created_at: time::OffsetDateTime::now_utc(),
            updated_at: time::OffsetDateTime::now_utc(),
            name: "Ada".to_string(),
        },
    )
    .await
    .expect("create Ada");
    let engineers = M2mGroup::create(
        &mut ctx,
        M2mGroup {
            id: <djogi::types::HeerIdDesc as djogi::PrimaryKey>::sentinel(),
            created_at: time::OffsetDateTime::now_utc(),
            updated_at: time::OffsetDateTime::now_utc(),
            name: "Engineers".to_string(),
        },
    )
    .await
    .expect("create Engineers");
    let reviewers = M2mGroup::create(
        &mut ctx,
        M2mGroup {
            id: <djogi::types::HeerIdDesc as djogi::PrimaryKey>::sentinel(),
            created_at: time::OffsetDateTime::now_utc(),
            updated_at: time::OffsetDateTime::now_utc(),
            name: "Reviewers".to_string(),
        },
    )
    .await
    .expect("create Reviewers");
    let _ = M2mPersonGroup::create(
        &mut ctx,
        M2mPersonGroup {
            id: <djogi::types::HeerIdDesc as djogi::PrimaryKey>::sentinel(),
            created_at: time::OffsetDateTime::now_utc(),
            updated_at: time::OffsetDateTime::now_utc(),
            person_id: ForeignKey::new(ada.id),
            group_id: ForeignKey::new(engineers.id),
            role: "member".to_string(),
        },
    )
    .await
    .expect("join Ada → Engineers");
    let _ = M2mPersonGroup::create(
        &mut ctx,
        M2mPersonGroup {
            id: <djogi::types::HeerIdDesc as djogi::PrimaryKey>::sentinel(),
            created_at: time::OffsetDateTime::now_utc(),
            updated_at: time::OffsetDateTime::now_utc(),
            person_id: ForeignKey::new(ada.id),
            group_id: ForeignKey::new(reviewers.id),
            role: "lead".to_string(),
        },
    )
    .await
    .expect("join Ada → Reviewers");

    // Drive the visage-scoped M2M accessor. b returns
    // a SELECT-narrowed `VisageQuerySet<M2mGroupPublic>`.
    let ada_public = M2mPersonPublic::from(&ada);
    let groups_qs = ada_public.groups();

    let mut groups: Vec<M2mGroupPublic> = groups_qs
        .fetch_all(&mut ctx)
        .await
        .expect("m2m visage accessor");
    groups.sort_by(|a, b| a.name.cmp(&b.name));
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0].name, "Engineers");
    assert_eq!(groups[1].name, "Reviewers");
}
