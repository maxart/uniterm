//! Phase 0 two-runtime demo: drive the mio<->tokio boundary end to end.
//!
//! Run: `cargo run -p uniterm-server --example runtime_demo`
//!
//! It sends a few `CoreToAgent` messages from the (sync) core loop, lets the
//! (async) agent runtime handle them and wake the core via the mio Waker, and
//! prints the replies. Then it shows the core loop blocking with no work and
//! being woken only by an actual message - the zero-idle-wakeup property.

use std::time::{Duration, Instant};

use uniterm_proto::{CoreToAgent, PaneId};
use uniterm_server::runtime::CoreLoop;

fn main() -> std::io::Result<()> {
    println!("== Uniterm two-runtime demo ==\n");

    let mut core = CoreLoop::new()?;

    // 1. Fire three agent events from the hot-path side.
    for pane in [1u64, 2, 3] {
        core.send_to_agent(CoreToAgent::OscAgentEvent {
            pane: PaneId(pane),
            payload: r#"{"event":"prompt_submit"}"#.into(),
        });
    }
    core.send_to_agent(CoreToAgent::PtyExited { pane: PaneId(2) });

    // 2. Pump the loop until we've seen all replies (bounded so we can't hang).
    let mut seen = 0;
    let deadline = Instant::now() + Duration::from_secs(2);
    while seen < 4 && Instant::now() < deadline {
        let replies = core.tick(Some(Duration::from_millis(200)))?;
        for r in replies {
            println!("core received from agent runtime: {r:?}");
            seen += 1;
        }
    }
    println!("\nhandled {seen} replies across the mio<->tokio boundary");

    // 3. Demonstrate idle quiescence: with nothing pending, a tick with a short
    //    timeout returns empty and burns no CPU (in production the timeout is
    //    None = sleep forever until woken).
    let t = Instant::now();
    let idle = core.tick(Some(Duration::from_millis(100)))?;
    println!(
        "idle tick returned {} messages after {:?} (loop was asleep, not spinning)",
        idle.len(),
        t.elapsed()
    );

    println!("\nOK");
    Ok(())
}
