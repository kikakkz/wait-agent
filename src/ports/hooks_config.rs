use std::io;

pub trait HooksConfigPort: Send + Sync {
    fn agent_name(&self) -> &'static str;
    fn reconcile(&self) -> io::Result<()>;
}
