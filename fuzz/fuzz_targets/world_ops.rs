#![no_main]
//! Drives random World operations and checks them against a simple model.
use canopy_ecs::{Component, Entity, World};
use libfuzzer_sys::fuzz_target;
use std::collections::HashMap;

#[derive(Debug, PartialEq, Clone)]
struct A(u8);
impl Component for A {}
#[derive(Debug, PartialEq, Clone)]
struct B(String);
impl Component for B {}
struct Z;
impl Component for Z {}

fuzz_target!(|data: &[u8]| {
    let mut w = World::new();
    let mut model: HashMap<Entity, (Option<u8>, Option<String>, bool)> = HashMap::new();
    let mut handles: Vec<Entity> = Vec::new();
    for ch in data.chunks(2) {
        let (op, arg) = (ch[0], *ch.get(1).unwrap_or(&0));
        let pick = (!handles.is_empty()).then(|| handles[arg as usize % handles.len()]);
        match op % 8 {
            0 => {
                let e = w.spawn(A(arg));
                model.insert(e, (Some(arg), None, false));
                handles.push(e);
            }
            1 => if let Some(e) = pick { assert_eq!(w.despawn(e), model.remove(&e).is_some()); },
            2 => if let Some(e) = pick {
                assert_eq!(w.insert(e, B(arg.to_string())).is_ok(), model.contains_key(&e));
                if let Some(m) = model.get_mut(&e) { m.1 = Some(arg.to_string()); }
            },
            3 => if let Some(e) = pick {
                let _ = w.insert(e, (Z, A(arg)));
                if let Some(m) = model.get_mut(&e) { m.2 = true; m.0 = Some(arg); }
            },
            4 => if let Some(e) = pick {
                let r = w.remove::<A>(e).ok();
                assert_eq!(r, model.get_mut(&e).map(|m| m.0.take().is_some()));
            },
            5 => if let Some(e) = pick {
                let r = w.remove::<B>(e).ok();
                assert_eq!(r, model.get_mut(&e).map(|m| m.1.take().is_some()));
            },
            6 => if let Some(e) = pick {
                let r = w.remove::<Z>(e).ok();
                assert_eq!(r, model.get_mut(&e).map(|m| std::mem::replace(&mut m.2, false)));
            },
            _ => {
                for mut a in w.query::<&mut A, ()>().unwrap() {
                    a.0 = a.0.wrapping_add(1);
                }
                for m in model.values_mut() {
                    if let Some(v) = &mut m.0 { *v = v.wrapping_add(1); }
                }
            }
        }
    }
    assert_eq!(w.entity_count(), model.len());
    for (e, (a, b, z)) in &model {
        assert_eq!(w.get::<A>(*e).map(|x| x.0), *a);
        assert_eq!(w.get::<B>(*e).map(|x| x.0.clone()), *b);
        assert_eq!(w.get::<Z>(*e).is_some(), *z);
    }
});
