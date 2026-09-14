use std::sync::{Arc, atomic::AtomicBool};

use anyhow::{Result, bail};

use crate::cli::SelfUpdateArgs;
use crate::config::AppConfig;
use crate::sync::wire::SYNC_PROTOCOL_VERSION;
use crate::update::{
    self, CheckOutcome, CompatibilityFailure, CompatibilityResult, ConfiguredSyncServer, Release,
};

pub(crate) async fn run(args: SelfUpdateArgs) -> Result<()> {
    let client = update::client()?;
    println!("Checking for aven updates...");
    let (release, cached) = match update::check_for_update(&client, true).await {
        Ok(CheckOutcome::Current { version, cached }) => {
            println!(
                "aven v{} is current (latest release v{version}){}",
                update::CURRENT_VERSION,
                cached_suffix(cached)
            );
            return Ok(());
        }
        Ok(CheckOutcome::Available { release, cached }) => (release, cached),
        Err(error) => match update::cached_update() {
            Some(release) => (release, true),
            None => return Err(error),
        },
    };

    install_release(
        client,
        release,
        cached,
        args.yes,
        args.allow_sync_incompatibility,
    )
    .await
}

async fn install_release(
    client: reqwest::Client,
    release: Release,
    cached: bool,
    yes: bool,
    allow_sync_incompatibility: bool,
) -> Result<()> {
    let plan = update::install_plan(release.clone());
    if let Some(lines) = plan.guidance() {
        for line in lines {
            println!("{line}");
        }
        if cached {
            println!("Release information came from the local cache.");
        }
        return Ok(());
    }

    let target = plan
        .direct_target()
        .expect("direct update plan must have a target");
    if !yes {
        println!(
            "aven v{} can update to v{} at {}{}",
            update::CURRENT_VERSION,
            release.version,
            target.display(),
            cached_suffix(cached)
        );
        println!("Run `aven update --yes` to install it.");
        return Ok(());
    }

    let (compatibility, server_origin) = assess_for_cli(&release).await;
    report_compatibility(
        &compatibility,
        server_origin.as_deref(),
        allow_sync_incompatibility,
    )?;

    let (progress_tx, mut progress_rx) = update::progress_channel();
    let cancelled = Arc::new(AtomicBool::new(false));
    let install = update::install_direct(client, plan, progress_tx, cancelled);
    tokio::pin!(install);
    let mut phase = progress_rx.borrow().phase;
    println!("{}...", phase.label());

    let success = loop {
        tokio::select! {
            result = &mut install => break result?,
            changed = progress_rx.changed() => {
                if changed.is_err() {
                    continue;
                }
                let next = progress_rx.borrow().phase;
                if next != phase {
                    phase = next;
                    println!("{}...", phase.label());
                }
            }
        }
    };

    println!("Installed aven v{}.", success.version);
    println!("Restart any running aven processes to use the update.");
    Ok(())
}

async fn assess_for_cli(release: &Release) -> (CompatibilityResult, Option<String>) {
    if release.sync_protocol == Some(SYNC_PROTOCOL_VERSION) {
        return (CompatibilityResult::NotRequired, None);
    }
    let config = match AppConfig::load() {
        Ok(config) => config,
        Err(_) => {
            return (
                CompatibilityResult::Unverified {
                    target: release.sync_protocol,
                    reason: CompatibilityFailure::ConfigUnreadable,
                },
                None,
            );
        }
    };
    let server = ConfiguredSyncServer::from_config(&config);
    let origin = server.as_ref().map(ConfiguredSyncServer::origin);
    (
        update::assess_sync_compatibility(release.sync_protocol, server).await,
        origin,
    )
}

fn report_compatibility(
    result: &CompatibilityResult,
    server_origin: Option<&str>,
    allowed: bool,
) -> Result<()> {
    match result {
        CompatibilityResult::NotRequired | CompatibilityResult::Compatible => return Ok(()),
        CompatibilityResult::Incompatible { target, server } => {
            eprintln!(
                "Warning: this update speaks sync protocol {target}, but the configured server speaks protocol {server}."
            );
            eprintln!("After installation, this device will not sync until the server is updated.");
        }
        CompatibilityResult::Unverified { reason, .. } => {
            eprintln!("Warning: sync compatibility could not be verified.");
            eprintln!("{}", reason.explanation());
        }
    }
    if let Some(origin) = server_origin {
        eprintln!("Sync server: {origin}");
    }
    eprintln!("To keep sync working, update the sync server before installing this update.");
    if !allowed {
        bail!(
            "update not installed; rerun with --yes --allow-sync-incompatibility to continue anyway"
        );
    }
    eprintln!("Continuing because --allow-sync-incompatibility was supplied.");
    Ok(())
}

fn cached_suffix(cached: bool) -> &'static str {
    if cached {
        " using cached release information"
    } else {
        ""
    }
}
