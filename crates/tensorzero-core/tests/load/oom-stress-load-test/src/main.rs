use std::{
    fs::File,
    io::Write,
    path::PathBuf,
    process::Command,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use clap::Parser;

#[derive(Debug, Parser)]
struct Args {
    /// Gateway process id to sample.
    #[arg(long)]
    pid: u32,

    /// Total sampling duration in seconds.
    #[arg(long, default_value_t = 600)]
    duration_seconds: u64,

    /// RSS sampling interval in seconds.
    #[arg(long, default_value_t = 30)]
    sample_seconds: u64,

    /// CSV output path.
    #[arg(long, default_value = "oom-stress-rss.csv")]
    output: PathBuf,

    /// File to create after baseline RSS has been sampled.
    #[arg(long)]
    ready_file: Option<PathBuf>,

    /// Maximum allowed RSS growth over baseline, in KiB.
    #[arg(long, default_value_t = 262_144)]
    max_growth_kib: u64,

    /// Maximum final RSS growth over baseline, as a percentage.
    #[arg(long, default_value_t = 10)]
    max_final_growth_percent: u64,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let sample_interval = Duration::from_secs(args.sample_seconds.max(1));
    let duration = Duration::from_secs(args.duration_seconds);

    let baseline = rss_kib(args.pid).with_context(|| {
        format!(
            "failed to read baseline RSS for gateway process {}",
            args.pid
        )
    })?;
    let mut max_rss = baseline;
    let mut final_rss = baseline;
    let started = Instant::now();

    let mut output = File::create(&args.output)
        .with_context(|| format!("failed to create {}", args.output.display()))?;
    writeln!(output, "elapsed_seconds,rss_kib,delta_kib")?;
    writeln!(output, "0,{baseline},0")?;
    output.flush()?;
    if let Some(ready_file) = &args.ready_file {
        File::create(ready_file)
            .with_context(|| format!("failed to create {}", ready_file.display()))?;
    }

    while started.elapsed() < duration {
        thread::sleep(sample_interval);
        let elapsed = started.elapsed().as_secs();
        let rss = rss_kib(args.pid)
            .with_context(|| format!("failed to read RSS for gateway process {}", args.pid))?;
        max_rss = max_rss.max(rss);
        final_rss = rss;
        writeln!(output, "{elapsed},{rss},{}", rss.saturating_sub(baseline))?;
    }

    let growth = max_rss.saturating_sub(baseline);
    let final_growth = final_rss.saturating_sub(baseline);
    let final_growth_budget = baseline.saturating_mul(args.max_final_growth_percent) / 100;
    println!("baseline_rss_kib={baseline}");
    println!("max_rss_kib={max_rss}");
    println!("max_growth_kib={growth}");
    println!("final_rss_kib={final_rss}");
    println!("final_growth_kib={final_growth}");

    if growth > args.max_growth_kib {
        bail!(
            "gateway RSS grew by {growth} KiB, above the {} KiB budget",
            args.max_growth_kib
        );
    }

    if final_growth > final_growth_budget {
        bail!(
            "gateway final RSS stayed {final_growth} KiB over baseline, above the {final_growth_budget} KiB budget ({}%)",
            args.max_final_growth_percent
        );
    }

    Ok(())
}

fn rss_kib(pid: u32) -> Result<u64> {
    let output = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .context("failed to invoke ps")?;
    if !output.status.success() {
        bail!("ps failed with status {}", output.status);
    }

    let stdout = String::from_utf8(output.stdout).context("ps output was not valid UTF-8")?;
    let rss = stdout
        .trim()
        .parse::<u64>()
        .with_context(|| format!("failed to parse RSS from ps output `{}`", stdout.trim()))?;
    Ok(rss)
}
