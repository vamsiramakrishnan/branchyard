use branchyard_controls::resume::{plan, AgentSessionRef};

fn main() -> Result<(), &'static str> {
    let session = AgentSessionRef::id("example-session").ok_or("invalid session ID")?;
    let recipe = plan("herdr:codex", "codex", &session).ok_or("unsupported resume recipe")?;
    // This example prints arguments. It does not launch or authorize a harness.
    println!("{:?}", recipe.argv);
    Ok(())
}
