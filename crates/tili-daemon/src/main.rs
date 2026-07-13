mod dispatch;

use dispatch::{dispatch, WmState};
use tili_ipc::Command;

#[tokio::main]
async fn main() {
    println!("tili-daemon: scaffolding only, event loop lands in M2");

    let mut state = WmState::default();
    let response = dispatch(&mut state, Command::Ping);
    println!("self-check dispatch(Ping) -> {response:?}");
}
