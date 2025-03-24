use yuki::app;
use std::env;
use tokio::sync::broadcast;

#[tokio::main]
async fn main() {
    let (shutdown, _) = broadcast::channel(1);
    let sigterm = tokio::signal::ctrl_c();

    let sigterm_shutdown = shutdown.clone();
    tokio::spawn(async move {
        sigterm.await.expect("could not listen for shutdown");
        let _ = sigterm_shutdown.send(());
    });

    if let Err(e) = app::run(env::args().collect(), shutdown).await {
        eprintln!("{}", e);
        safe_exit(1);
    }
}

// from clap utilities
pub fn safe_exit(code: i32) -> ! {
    use std::io::Write;

    let _ = std::io::stdout().lock().flush();
    let _ = std::io::stderr().lock().flush();

    std::process::exit(code)
}
