//! Property test: random operation sequences vs. a naive HashMap model.
use canopy_ecs::{Entity, World};
use proptest::prelude::*;
use std::any::{Any, TypeId};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq)]
struct A(u32);
impl canopy_ecs::Component for A {}
#[derive(Debug, Clone, Copy, PartialEq)]
struct B(u64);
impl canopy_ecs::Component for B {}
#[derive(Debug, Clone, PartialEq)]
struct C(String);
impl canopy_ecs::Component for C {}
/// Sparse-set component with heap data (so a lost or doubled drop shows up under Miri).
#[derive(Debug, Clone, PartialEq)]
struct S(String);
impl canopy_ecs::Component for S {
    const STORAGE: canopy_ecs::StorageType = canopy_ecs::StorageType::SparseSet;
}

#[derive(Debug, Clone)]
enum Op {
    Spawn(u32),
    Despawn(usize),
    InsertA(usize, u32),
    InsertB(usize, u64),
    InsertC(usize, u32),
    RemoveA(usize),
    RemoveB(usize),
    RemoveC(usize),
    InsertS(usize, u32),
    RemoveS(usize),
    MutateS(usize, u32),
    /// Bundle mixing heap table, sparse and plain table components.
    InsertCSA(usize, u32),
    /// Bundle with a duplicate sparse component: the later `S` wins.
    InsertSBS(usize, u32),
    /// Spawn with a duplicate heap table component: the later `C` wins.
    SpawnACSC(u32),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        any::<u32>().prop_map(Op::Spawn),
        any::<usize>().prop_map(Op::Despawn),
        (any::<usize>(), any::<u32>()).prop_map(|(i, v)| Op::InsertA(i, v)),
        (any::<usize>(), any::<u64>()).prop_map(|(i, v)| Op::InsertB(i, v)),
        (any::<usize>(), any::<u32>()).prop_map(|(i, v)| Op::InsertC(i, v)),
        any::<usize>().prop_map(Op::RemoveA),
        any::<usize>().prop_map(Op::RemoveB),
        any::<usize>().prop_map(Op::RemoveC),
        (any::<usize>(), any::<u32>()).prop_map(|(i, v)| Op::InsertS(i, v)),
        any::<usize>().prop_map(Op::RemoveS),
        (any::<usize>(), any::<u32>()).prop_map(|(i, v)| Op::MutateS(i, v)),
        (any::<usize>(), any::<u32>()).prop_map(|(i, v)| Op::InsertCSA(i, v)),
        (any::<usize>(), any::<u32>()).prop_map(|(i, v)| Op::InsertSBS(i, v)),
        any::<u32>().prop_map(Op::SpawnACSC),
    ]
}

type Model = HashMap<Entity, HashMap<TypeId, Box<dyn Any>>>;

fn check(w: &mut World, m: &Model, handles: &[Entity]) {
    assert_eq!(w.entity_count(), m.len());
    for (e, comps) in m {
        assert_eq!(
            w.get::<A>(*e),
            comps
                .get(&TypeId::of::<A>())
                .and_then(|b| b.downcast_ref::<A>())
        );
        assert_eq!(
            w.get::<B>(*e),
            comps
                .get(&TypeId::of::<B>())
                .and_then(|b| b.downcast_ref::<B>())
        );
        assert_eq!(
            w.get::<C>(*e),
            comps
                .get(&TypeId::of::<C>())
                .and_then(|b| b.downcast_ref::<C>())
        );
        assert_eq!(
            w.get::<S>(*e),
            comps
                .get(&TypeId::of::<S>())
                .and_then(|b| b.downcast_ref::<S>())
        );
    }
    // Queries mixing table and sparse storage agree with the model too.
    let mut q: Vec<(Entity, Option<u32>, String)> = w
        .query::<(Entity, Option<&A>, &S), ()>()
        .unwrap()
        .map(|(e, a, s)| (e, a.map(|a| a.0), s.0.clone()))
        .collect();
    q.sort();
    let mut expect: Vec<(Entity, Option<u32>, String)> = m
        .iter()
        .filter_map(|(e, c)| {
            let s = c.get(&TypeId::of::<S>())?.downcast_ref::<S>()?;
            let a = c
                .get(&TypeId::of::<A>())
                .and_then(|b| b.downcast_ref::<A>());
            Some((*e, a.map(|a| a.0), s.0.clone()))
        })
        .collect();
    expect.sort();
    assert_eq!(q, expect);
    // The sparse set holds exactly the live entities that have S: despawned and
    // stale-generation ids (same slot index, reused) are never reported as present.
    let has_s = |c: &HashMap<TypeId, Box<dyn Any>>| c.contains_key(&TypeId::of::<S>());
    let set = w
        .component_id(TypeId::of::<S>())
        .and_then(|id| w.sparse_set(id));
    assert_eq!(
        set.map_or(0, |s| s.len()),
        m.values().filter(|c| has_s(c)).count()
    );
    for h in handles {
        assert_eq!(
            set.is_some_and(|s| s.contains(*h)),
            m.get(h).is_some_and(has_s),
            "sparse membership of {h:?}"
        );
    }
    let without_s = w.query::<Entity, canopy_ecs::Without<S>>().unwrap().count();
    assert_eq!(
        without_s,
        m.values()
            .filter(|c| !c.contains_key(&TypeId::of::<S>()))
            .count()
    );
    let mut q: Vec<(Entity, u32)> = w
        .query::<(Entity, &A), ()>()
        .unwrap()
        .map(|(e, a)| (e, a.0))
        .collect();
    q.sort();
    let mut expect: Vec<(Entity, u32)> = m
        .iter()
        .filter_map(|(e, c)| {
            c.get(&TypeId::of::<A>())
                .and_then(|b| b.downcast_ref::<A>())
                .map(|a| (*e, a.0))
        })
        .collect();
    expect.sort();
    assert_eq!(q, expect);
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: if cfg!(miri) { 4 } else { 256 },
        failure_persistence: None,
        ..ProptestConfig::default()
    })]
    #[test]
    fn world_matches_model(ops in proptest::collection::vec(op(), 0..if cfg!(miri) { 30 } else { 200 })) {
        let mut w = World::new();
        let mut m: Model = HashMap::new();
        let mut handles: Vec<Entity> = Vec::new();
        for op in ops {
            let pick = |i: usize| (!handles.is_empty()).then(|| handles[i % handles.len()]);
            match op {
                Op::Spawn(v) => {
                    let e = w.spawn(A(v));
                    let mut c: HashMap<TypeId, Box<dyn Any>> = HashMap::new();
                    c.insert(TypeId::of::<A>(), Box::new(A(v)));
                    m.insert(e, c);
                    handles.push(e);
                }
                Op::Despawn(i) => if let Some(e) = pick(i) {
                    prop_assert_eq!(w.despawn(e), m.remove(&e).is_some());
                },
                Op::InsertA(i, v) => if let Some(e) = pick(i) {
                    prop_assert_eq!(w.insert(e, A(v)).is_ok(), m.contains_key(&e));
                    if let Some(c) = m.get_mut(&e) { c.insert(TypeId::of::<A>(), Box::new(A(v))); }
                },
                Op::InsertB(i, v) => if let Some(e) = pick(i) {
                    prop_assert_eq!(w.insert(e, B(v)).is_ok(), m.contains_key(&e));
                    if let Some(c) = m.get_mut(&e) { c.insert(TypeId::of::<B>(), Box::new(B(v))); }
                },
                Op::InsertC(i, v) => if let Some(e) = pick(i) {
                    prop_assert_eq!(w.insert(e, C(v.to_string())).is_ok(), m.contains_key(&e));
                    if let Some(c) = m.get_mut(&e) { c.insert(TypeId::of::<C>(), Box::new(C(v.to_string()))); }
                },
                Op::RemoveA(i) => if let Some(e) = pick(i) {
                    let r = w.remove::<A>(e).ok();
                    prop_assert_eq!(r, m.get_mut(&e).map(|c| c.remove(&TypeId::of::<A>()).is_some()));
                },
                Op::RemoveB(i) => if let Some(e) = pick(i) {
                    let r = w.remove::<B>(e).ok();
                    prop_assert_eq!(r, m.get_mut(&e).map(|c| c.remove(&TypeId::of::<B>()).is_some()));
                },
                Op::RemoveC(i) => if let Some(e) = pick(i) {
                    let r = w.remove::<C>(e).ok();
                    prop_assert_eq!(r, m.get_mut(&e).map(|c| c.remove(&TypeId::of::<C>()).is_some()));
                },
                Op::InsertS(i, v) => if let Some(e) = pick(i) {
                    prop_assert_eq!(w.insert(e, S(v.to_string())).is_ok(), m.contains_key(&e));
                    if let Some(c) = m.get_mut(&e) { c.insert(TypeId::of::<S>(), Box::new(S(v.to_string()))); }
                },
                Op::RemoveS(i) => if let Some(e) = pick(i) {
                    let r = w.remove::<S>(e).ok();
                    prop_assert_eq!(r, m.get_mut(&e).map(|c| c.remove(&TypeId::of::<S>()).is_some()));
                },
                // Writes through `Mut<S>` in a query must land on the right entity.
                Op::MutateS(i, v) => if let Some(e) = pick(i) {
                    for (qe, mut s) in w.query::<(Entity, &mut S), ()>().unwrap() {
                        if qe == e { s.0 = v.to_string(); }
                    }
                    if let Some(s) = m.get_mut(&e).and_then(|c| c.get_mut(&TypeId::of::<S>())) {
                        *s = Box::new(S(v.to_string()));
                    }
                },
                Op::InsertCSA(i, v) => if let Some(e) = pick(i) {
                    let ok = w.insert(e, (C(v.to_string()), S(format!("s{v}")), A(v))).is_ok();
                    prop_assert_eq!(ok, m.contains_key(&e));
                    if let Some(c) = m.get_mut(&e) {
                        c.insert(TypeId::of::<C>(), Box::new(C(v.to_string())));
                        c.insert(TypeId::of::<S>(), Box::new(S(format!("s{v}"))));
                        c.insert(TypeId::of::<A>(), Box::new(A(v)));
                    }
                },
                Op::InsertSBS(i, v) => if let Some(e) = pick(i) {
                    let ok = w.insert(e, (S(v.to_string()), B(u64::from(v)), S(format!("{v}!")))).is_ok();
                    prop_assert_eq!(ok, m.contains_key(&e));
                    if let Some(c) = m.get_mut(&e) {
                        c.insert(TypeId::of::<B>(), Box::new(B(u64::from(v))));
                        c.insert(TypeId::of::<S>(), Box::new(S(format!("{v}!"))));
                    }
                },
                Op::SpawnACSC(v) => {
                    let e = w.spawn((A(v), C(v.to_string()), S(v.to_string()), C(format!("{v}!"))));
                    let mut c: HashMap<TypeId, Box<dyn Any>> = HashMap::new();
                    c.insert(TypeId::of::<A>(), Box::new(A(v)));
                    c.insert(TypeId::of::<C>(), Box::new(C(format!("{v}!"))));
                    c.insert(TypeId::of::<S>(), Box::new(S(v.to_string())));
                    m.insert(e, c);
                    handles.push(e);
                },
            }
        }
        check(&mut w, &m, &handles);
    }
}

// ---- Model check: disjointness claims vs. actual archetype matching -----------
//
// For a menu of query shapes, every pair the access model declares disjoint must
// in fact match no common entity, in a world holding one entity for each of the 8
// archetypes over {A, B, C}. This is the soundness property the scheduler and the
// in-system parameter check rely on.

struct Shape {
    name: &'static str,
    fa: canopy_ecs::query::FilteredAccess,
    ents: std::collections::HashSet<Entity>,
}

fn shape<D: canopy_ecs::QueryData + 'static, F: canopy_ecs::QueryFilter + 'static>(
    w: &mut World,
    name: &'static str,
) -> Shape {
    Shape {
        name,
        fa: canopy_ecs::query::FilteredAccess::of::<D, F>().unwrap(),
        ents: w
            .query::<(Entity, D), F>()
            .unwrap()
            .map(|(e, _)| e)
            .collect(),
    }
}

#[test]
fn disjointness_claims_match_archetype_reality() {
    use canopy_ecs::{Added, Changed, Or, With, Without};
    let mut w = World::new();
    for mask in 0u8..16 {
        let e = w.spawn_empty();
        if mask & 1 != 0 {
            w.insert(e, A(0)).unwrap();
        }
        if mask & 2 != 0 {
            w.insert(e, B(0)).unwrap();
        }
        if mask & 4 != 0 {
            w.insert(e, C(String::new())).unwrap();
        }
        if mask & 8 != 0 {
            w.insert(e, S(String::new())).unwrap();
        }
    }
    let shapes = vec![
        shape::<&mut A, ()>(&mut w, "A"),
        shape::<&mut A, With<B>>(&mut w, "A with B"),
        shape::<&mut A, Without<B>>(&mut w, "A without B"),
        shape::<&A, (With<B>, Without<C>)>(&mut w, "A with B without C"),
        shape::<&mut A, Or<(With<B>, Without<B>)>>(&mut w, "A or(B, !B)"),
        shape::<&mut A, Or<(Without<B>, With<C>)>>(&mut w, "A or(!B, C)"),
        shape::<&mut A, Or<(With<B>, With<C>)>>(&mut w, "A or(B, C)"),
        shape::<(&mut A, Option<&B>), ()>(&mut w, "A opt B"),
        shape::<(&mut A, &B), ()>(&mut w, "A, &B"),
        shape::<&mut A, Changed<B>>(&mut w, "A changed B"),
        shape::<&mut A, Added<C>>(&mut w, "A added C"),
        shape::<(Entity, &mut B), Without<A>>(&mut w, "B without A"),
        shape::<&mut A, (Without<B>, Without<C>)>(&mut w, "A without B, C"),
        shape::<(&mut A, Option<&C>), Without<B>>(&mut w, "A opt C without B"),
        shape::<&mut C, With<A>>(&mut w, "C with A"),
        shape::<&mut B, ()>(&mut w, "B"),
        shape::<&mut C, Or<(Without<A>, With<B>)>>(&mut w, "C or(!A, B)"),
        shape::<Option<&A>, Without<C>>(&mut w, "opt A without C"),
        shape::<&mut B, (With<C>, Without<C>)>(&mut w, "B with C without C"),
        // Sparse-set component S: same rules, though all S values share one column.
        shape::<&mut S, ()>(&mut w, "S"),
        shape::<&mut S, With<A>>(&mut w, "S with A"),
        shape::<(&mut S, &B), ()>(&mut w, "S, &B"),
        shape::<&S, Without<A>>(&mut w, "S without A"),
        shape::<&mut A, With<S>>(&mut w, "A with S"),
        shape::<&mut A, Without<S>>(&mut w, "A without S"),
        shape::<(&mut A, Option<&S>), ()>(&mut w, "A opt S"),
        shape::<&mut A, Changed<S>>(&mut w, "A changed S"),
        shape::<&mut S, Or<(With<A>, Without<B>)>>(&mut w, "S or(A, !B)"),
    ];
    let mut proven = 0;
    for x in &shapes {
        for y in &shapes {
            if x.fa.is_disjoint(&y.fa) {
                proven += 1;
                assert!(
                    x.ents.is_disjoint(&y.ents),
                    "`{}` and `{}` declared disjoint but share {:?}",
                    x.name,
                    y.name,
                    x.ents.intersection(&y.ents).collect::<Vec<_>>()
                );
                assert!(y.fa.is_disjoint(&x.fa), "disjointness must be symmetric");
            }
            if x.fa.access().conflicts(y.fa.access()) && !x.ents.is_disjoint(&y.ents) {
                assert!(x.fa.conflicts(&y.fa), "`{}` / `{}`", x.name, y.name);
            }
        }
    }
    // Pin the expected precision too, so the rule cannot silently become "never
    // disjoint" (which is sound but useless).
    let get = |n: &str| shapes.iter().find(|s| s.name == n).unwrap();
    let disjoint = |a: &str, b: &str| get(a).fa.is_disjoint(&get(b).fa);
    assert!(disjoint("A with B", "A without B"));
    assert!(disjoint("A, &B", "A without B"));
    assert!(disjoint("A changed B", "A without B, C"));
    assert!(disjoint("C with A", "B without A"));
    assert!(disjoint("A added C", "opt A without C"));
    assert!(disjoint("S with A", "S without A"));
    assert!(disjoint("A with S", "A without S"));
    assert!(disjoint("A changed S", "A without S"));
    assert!(!disjoint("A opt S", "A without S"));
    assert!(!disjoint("S", "S without A"));
    assert!(!disjoint("S or(A, !B)", "S without A"));
    // Truly disjoint, but only via `Or`: conservatively not proven.
    assert!(!disjoint("A or(B, C)", "A without B, C"));
    // `Option` never implies presence.
    assert!(!disjoint("A opt B", "A without B"));
    assert!(proven > 0);
}
