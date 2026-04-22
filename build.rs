#[path = "src/infra/tmux_glue_build_script.rs"]
mod tmux_glue_build_script;
#[path = "src/infra/tmux_glue_contract.rs"]
mod tmux_glue_contract;

fn main() {
    tmux_glue_build_script::run();
}
