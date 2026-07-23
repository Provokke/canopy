// Two live mutable iterators over the same world cannot coexist.
struct A(u32);
impl canopy_ecs::Component for A {}
fn main() {
    let mut w = canopy_ecs::World::new();
    let a = w.query::<&mut A, ()>().unwrap();
    let b = w.query::<&mut A, ()>().unwrap();
    drop((a, b));
}
