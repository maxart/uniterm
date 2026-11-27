use ut_compare::{adapters::AppAdapter, config::Config};
fn main() -> Result<(), String> {
    let config = Config::load(std::path::Path::new("startup.local.toml"))?;
    let mut samples = Vec::new();
    for trial in 0..20 {
        let order = if trial % 2 == 0 { [0, 1] } else { [1, 0] };
        for (position, index) in order.into_iter().enumerate() {
            let contender = config.contenders[index].clone();
            let adapter = AppAdapter::new(contender.clone(), 160, 50);
            let mut started = adapter.start(trial, false)?;
            let startup_ms = started.startup_elapsed.as_secs_f64() * 1000.0;
            let shutdown_ms = started.session.stop()?.as_secs_f64() * 1000.0;
            samples.push(serde_json::json!({"trial": trial, "position": position,
                "contender": contender.id, "startup_ms": startup_ms, "shutdown_ms": shutdown_ms}));
        }
    }
    let report = serde_json::json!({"scenario": "Fresh workspace startup using the unchanged ut-compare AppAdapter::start readiness probe", "trials_per_version": 20, "order": "Alternates each paired trial", "geometry": [160,50], "samples": samples});
    std::fs::write("reports/startup-followup.json", serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    Ok(())
}
