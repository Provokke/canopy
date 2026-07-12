// A component must be Send + Sync because systems share it across threads.
struct NotSend(std::rc::Rc<u32>);
impl canopy_ecs::Component for NotSend {}
fn main() {}
