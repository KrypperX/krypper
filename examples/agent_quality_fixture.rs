#[path = "../tests/agent_quality/mod.rs"]
mod agent_quality;

use agent_quality::fixture;
use agent_quality::harness::{self, FixtureRequest};
use std::fs;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = (|| -> Result<(), String> {
        if args.len() != 4 {
            return Err("usage: agent_quality_fixture --config <path> --out <path>".to_owned());
        }
        let config_path = harness::option_value(&args, "--config")?;
        let out_path = harness::option_value(&args, "--out")?;
        let bytes = fs::read(&config_path).map_err(|error| format!("read request: {error}"))?;
        let request: FixtureRequest =
            serde_json::from_slice(&bytes).map_err(|error| format!("parse request: {error}"))?;
        let results = fixture::run(&request)?;
        let mut output = serde_json::to_vec_pretty(&results)
            .map_err(|error| format!("encode fixture results: {error}"))?;
        output.push(b'\n');
        fs::write(&out_path, output).map_err(|error| format!("write fixture results: {error}"))?;
        Ok(())
    })();

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("agent_quality_fixture: {error}");
            ExitCode::FAILURE
        }
    }
}
