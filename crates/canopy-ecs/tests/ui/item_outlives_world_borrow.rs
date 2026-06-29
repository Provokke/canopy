// A `&mut` item (`Mut<A>`) cannot outlive the world borrow, so it cannot alias later access.
struct A(u32);
impl canopy_ecs::Component for A {}
fn main() {
    let mut w = canopy_ecs::World::new();
    let e = w.spawn(A(1));
    let mut item = w.query::<&mut A, ()>().unwrap().next().unwrap();
    w.despawn(e);
    item.0 = 2;
}
