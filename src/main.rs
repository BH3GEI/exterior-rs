//! exterior: Yao's Telegram topics ↔ agents on his devices, with one shared memory about him.
mod agents;
mod brief;
mod crypt;
mod daemon;
mod host;
mod memory;
mod prompt;
mod store;
mod web;

fn main() {
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(3000);
    let _ = std::fs::create_dir_all(store::var());
    daemon::start();
    web::serve(port);
}
