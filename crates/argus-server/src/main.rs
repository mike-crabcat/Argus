use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use argus_server::config::Config;
use argus_server::live::{Live, LiveStream};
use argus_server::recorder;
use argus_server::{http, reolink, s3, timearg, triggers};
use argus_store::{ChunkWriter, Index, Uploader, upload};
use clap::{Parser, Subcommand};
use tracing::{info, warn};

#[derive(Parser)]
#[command(version, about = "Argus NVR server")]
struct Args {
    /// Path to argus.toml.
    #[arg(short, long, default_value = "argus.toml", global = true)]
    config: PathBuf,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Record, upload and manage local disk (the default).
    Run,
    /// Manage events (periods worth keeping at full resolution).
    #[command(subcommand)]
    Event(EventCommand),
    /// Exit 0 if the server from this config answers /healthz (for Docker's
    /// HEALTHCHECK; the image has no curl).
    Healthcheck,
}

#[derive(Subcommand)]
enum EventCommand {
    /// Mark a period on one camera as an event.
    Add {
        #[arg(long)]
        camera: String,
        /// Start: RFC 3339 (2026-11-16T21:30:00Z), Unix ms, or now[-+]N[smh].
        #[arg(long, allow_hyphen_values = true)]
        from: String,
        /// End, in the same formats.
        #[arg(long, allow_hyphen_values = true)]
        to: String,
        #[arg(long, default_value = "manual")]
        label: String,
    },
    /// Read each camera's motion/AI detections for a while and print what
    /// they report, without recording or storing anything.
    Watch {
        #[arg(long, default_value_t = 10)]
        seconds: u64,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,retina=warn".into()),
        )
        .init();
    let args = Args::parse();
    let cfg = Config::load(&args.config)?;
    match args.command.unwrap_or(Command::Run) {
        Command::Run => run(cfg).await,
        Command::Healthcheck => healthcheck(&cfg).await,
        Command::Event(EventCommand::Add {
            camera,
            from,
            to,
            label,
        }) => {
            if !cfg.cameras.iter().any(|c| c.name == camera) {
                warn!("camera {camera:?} is not in the config");
            }
            let now = upload::now_ms();
            let (from, to) = (timearg::parse(&from, now)?, timearg::parse(&to, now)?);
            let index = Index::open(&cfg.storage.root.join("index.sqlite"))?;
            let id = index.add_event(&camera, from, to, "cli", &label)?;
            println!("event {id}: {camera} {from}..{to} ms ({label})");
            Ok(())
        }
        Command::Event(EventCommand::Watch { seconds }) => watch(&cfg, seconds).await,
    }
}

async fn watch(cfg: &Config, seconds: u64) -> Result<()> {
    let mut tasks = Vec::new();
    for cam in &cfg.cameras {
        let Some(ev) = cam.reolink_events()? else {
            println!("{}: no camera events configured", cam.name);
            continue;
        };
        let name = cam.name.clone();
        let mut client = reolink::Client::new(&ev.base, &ev.user, &ev.password)?;
        tasks.push(tokio::spawn(async move {
            let end = tokio::time::Instant::now() + Duration::from_secs(seconds);
            let mut polls = 0;
            let mut seen = std::collections::BTreeSet::new();
            let mut supported = Vec::new();
            let mut error = None;
            while tokio::time::Instant::now() < end {
                match client.detections().await {
                    Ok(d) => {
                        polls += 1;
                        supported = d.supported.clone();
                        seen.extend(d.active());
                    }
                    Err(e) => {
                        error = Some(format!("{e:#}"));
                        break;
                    }
                }
                tokio::time::sleep(ev.poll).await;
            }
            client.logout().await;
            match error {
                Some(e) => println!("{name}: unavailable: {e}"),
                None => println!(
                    "{name}: {polls} polls, AI types supported: {supported:?}, active during watch: {seen:?}"
                ),
            }
        }));
    }
    for t in tasks {
        t.await?;
    }
    Ok(())
}

async fn healthcheck(cfg: &Config) -> Result<()> {
    let h = cfg.http.as_ref().context("no [http] section to check")?;
    let mut addr = h.listen;
    if addr.ip().is_unspecified() {
        addr.set_ip(std::net::Ipv4Addr::LOCALHOST.into());
    }
    let resp = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()?
        .get(format!("http://{addr}/healthz"))
        .send()
        .await?;
    anyhow::ensure!(resp.status().is_success(), "healthz: {}", resp.status());
    Ok(())
}

async fn run(cfg: Config) -> Result<()> {
    std::fs::create_dir_all(&cfg.storage.root)
        .with_context(|| format!("creating {}", cfg.storage.root.display()))?;
    let index = Arc::new(Mutex::new(Index::open(
        &cfg.storage.root.join("index.sqlite"),
    )?));

    let mut ingest = Vec::new();
    let mut recorders = Vec::new();
    let mut live = Live::default();
    for cam in &cfg.cameras {
        for (stream, source) in &cam.streams {
            let out = live.add(LiveStream::new(&cam.name, stream, 1024));
            let rx = out.subscribe();
            let writer = ChunkWriter::new(
                &cfg.storage.root,
                &cam.name,
                stream,
                cfg.storage.chunk_seconds,
                index.clone(),
            );
            recorders.push(tokio::task::spawn_blocking(move || {
                recorder::record(rx, writer)
            }));
            ingest.push(tokio::spawn(recorder::supervise(
                cam.name.clone(),
                stream.clone(),
                source.clone(),
                out,
            )));
        }
    }
    info!(streams = ingest.len(), root = %cfg.storage.root.display(), "recording");
    let live = Arc::new(live);
    // Stills when an event starts, from the camera's Snap API on a session
    // of its own: for the camera's events and for Argus's.
    let mut still_starters: std::collections::HashMap<String, argus_server::detect::OnEvent> =
        Default::default();
    let rt = tokio::runtime::Handle::current();
    for cam in &cfg.cameras {
        let Some(ev) = cam.reolink_events()? else {
            continue;
        };
        if !ev.stills {
            continue;
        }
        let snaps: Arc<dyn argus_server::stills::StillSource> =
            Arc::new(argus_server::stills::ReolinkSnaps::new(
                reolink::Client::new(&ev.base, &ev.user, &ev.password)?,
            ));
        let store = argus_server::stills::StillStore::new(&cfg.storage.root);
        let name = cam.name.clone();
        let rt = rt.clone();
        still_starters.insert(
            cam.name.clone(),
            Arc::new(move |id| {
                rt.spawn(argus_server::stills::capture(
                    store.clone(),
                    snaps.clone(),
                    name.clone(),
                    id,
                    &argus_server::stills::OFFSETS_MS,
                ));
            }),
        );
    }
    let mut detect_events = std::collections::HashSet::new();
    if let Some(d) = &cfg.detect {
        let mut tracking = argus_server::detect::Tracking::new(index.clone());
        for c in &cfg.cameras {
            if d.cameras.is_empty() || d.cameras.contains(&c.name) {
                let mode = c.detect_mode()?;
                if mode == argus_server::detect::Mode::Events && d.model.is_some() {
                    detect_events.insert(c.name.clone());
                }
                tracking.camera(
                    &c.name,
                    mode,
                    c.hold(),
                    still_starters.get(&c.name).cloned(),
                );
            }
        }
        let streams: Vec<_> = cfg
            .cameras
            .iter()
            .filter(|c| d.cameras.is_empty() || d.cameras.contains(&c.name))
            .filter_map(|c| live.get(&c.name, &d.stream))
            .collect();
        info!(cameras = streams.len(), stream = %d.stream, fps = d.fps, "detection: decoding");
        let model = match &d.model {
            Some(path) => Some(argus_server::detect::ModelSpec {
                path: path.clone(),
                device: d.device()?,
                threshold: d.threshold,
                pose: d.pose.clone(),
            }),
            None => None,
        };
        argus_server::detect::set_report_every(d.log_seconds);
        argus_server::detect::spawn(streams, d.fps, model, tracking);
    }

    let mut background = Vec::new();
    if let Some(h) = &cfg.http {
        let listener = tokio::net::TcpListener::bind(h.listen)
            .await
            .with_context(|| format!("listening on {}", h.listen))?;
        let devices = Arc::new(argus_server::devices::Devices::open(&cfg.storage.root)?);
        {
            // Last-used times are kept in memory; write them out now and then.
            let devices = devices.clone();
            background.push(tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_secs(300));
                loop {
                    tick.tick().await;
                    if let Err(e) = devices.flush() {
                        warn!("saving devices: {e:#}");
                    }
                }
            }));
        }
        // Notifications, alert rules and the daily brief.
        let default_alerts = argus_server::config::AlertsConfig::default();
        let acfg = cfg.alerts.as_ref().unwrap_or(&default_alerts);
        let tz: chrono_tz::Tz = acfg
            .timezone
            .parse()
            .map_err(|e| anyhow::anyhow!("[alerts] timezone {:?}: {e}", acfg.timezone))?;
        let alerts = Arc::new(argus_server::alerts_api::AlertsState {
            rules: Arc::new(argus_server::alerts::RuleStore::open(&cfg.storage.root)?),
            notifier: Arc::new(argus_server::push::Notifier::new(
                &cfg.storage.root,
                acfg.ntfy.as_ref(),
                h.public_url.clone(),
                acfg.contact.clone(),
            )?),
            briefs: Arc::new(argus_server::brief::BriefStore::open(&cfg.storage.root)?),
            tz,
            brief_time: acfg.brief_time.clone(),
            brief_enabled: acfg.brief,
        });
        let state = http::AppState {
            live: live.clone(),
            token: h.token.as_deref().map(Arc::from),
            web_root: h.web_root.clone().map(Arc::new),
            recordings: Some(http::Recordings {
                index: index.clone(),
                root: Arc::new(cfg.storage.root.clone()),
                remote: match &cfg.s3 {
                    Some(s3cfg) => Some(s3::connect(s3cfg)?),
                    None => None,
                },
            }),
            devices: Some(devices),
            plan: Some(Arc::new(argus_server::plan::PlanStore::open(
                &cfg.storage.root,
            )?)),
            agent: cfg
                .agent
                .as_ref()
                .map(|a| Arc::new(argus_server::agent::AgentHandle::new(a, &cfg.storage.root))),
            alerts: Some(alerts.clone()),
            public_url: h
                .public_url
                .as_deref()
                .map(|u| Arc::from(u.trim_end_matches('/'))),
        };
        // Stills as models see them, and what each camera looks at.
        let model_stills = argus_server::stills::StillStore::new(&cfg.storage.root)
            .with_model_edge(
                cfg.agent
                    .as_ref()
                    .map_or(argus_server::stills::MODEL_EDGE, |a| a.still_max_edge),
            );
        let contexts: std::collections::HashMap<String, String> = cfg
            .cameras
            .iter()
            .filter_map(|c| Some((c.name.clone(), c.context.clone()?.trim().to_owned())))
            .filter(|(_, c)| !c.is_empty())
            .collect();
        // Event summaries, if a model is set up.
        if let (Some(handle), Some(ac)) = (&state.agent, &cfg.agent)
            && handle.agent.is_ok()
            && ac.summaries
        {
            let worker = argus_server::summaries::Worker {
                agent: handle.clone(),
                index: index.clone(),
                plan: state.plan.clone(),
                stills: model_stills.clone(),
                opted_out: cfg
                    .cameras
                    .iter()
                    .filter(|c| !c.summarize())
                    .map(|c| c.name.clone())
                    .collect(),
                gap_ms: ac.summary_gap_seconds as i64 * 1000,
                camera_gap_ms: cfg
                    .cameras
                    .iter()
                    .filter_map(|c| Some((c.name.clone(), c.summary_gap_seconds()? as i64 * 1000)))
                    .collect(),
                contexts: contexts.clone(),
                paused_until: 0,
            };
            info!(gap_s = ac.summary_gap_seconds, "summarising events");
            background.push(tokio::spawn(worker.run()));
        }
        let mut engine = argus_server::alerts::Engine::new(
            alerts.rules.clone(),
            index.clone(),
            alerts.notifier.clone(),
            state.agent.clone(),
            model_stills,
            tz,
        );
        engine.contexts = contexts;
        background.push(tokio::spawn(engine.run()));
        if alerts.brief_enabled {
            info!(at = %alerts.brief_time, timezone = %tz.name(), "daily brief");
            background.push(tokio::spawn(argus_server::brief::schedule(
                state.agent.clone(),
                index.clone(),
                alerts.briefs.clone(),
                alerts.notifier.clone(),
                alerts.brief_time.clone(),
                tz,
            )));
        }
        info!(listen = %h.listen, auth = h.token.is_some(), version = http::VERSION,
          "HTTP API and live view");
        background.push(tokio::spawn(async move {
            if let Err(e) = http::serve(listener, state).await {
                warn!("HTTP server stopped: {e}");
            }
        }));
    }
    for cam in &cfg.cameras {
        let Some(ev) = cam.reolink_events()? else {
            continue;
        };
        if detect_events.contains(&cam.name) {
            info!(camera = %cam.name, "events from Argus's own detection");
            continue;
        }
        info!(camera = %cam.name, api = %ev.base, labels = ?ev.labels,
              "events from camera detections");
        let client = reolink::Client::new(&ev.base, &ev.user, &ev.password)?;
        let mut t =
            triggers::ReolinkTriggers::new(&cam.name, client, ev.hold, ev.labels, index.clone());
        if let Some(start) = still_starters.get(&cam.name).cloned() {
            t = t.on_start(move |id| start(id));
        }
        background.push(tokio::spawn(t.run(ev.poll)));
    }

    if let Some(s3cfg) = &cfg.s3 {
        let store = s3::connect(s3cfg)?;
        let uploader = Uploader::new(
            store,
            index.clone(),
            &cfg.storage.root,
            &s3cfg.prefix,
            cfg.stream_policies(),
            s3cfg.max_upload_mbps,
        );
        info!(bucket = %s3cfg.bucket, prefix = %s3cfg.prefix,
              max_mbps = s3cfg.max_upload_mbps, "uploading");
        background.push(tokio::spawn(async move {
            if let Err(e) = uploader.run().await {
                warn!("uploader stopped: {e:#}");
            }
        }));
    }
    if let Some(gb) = cfg.storage.local_budget_gb {
        let budget = (gb * 1e9) as u64;
        let (index, root, cfg) = (index.clone(), cfg.storage.root.clone(), Arc::new(cfg));
        background.push(tokio::spawn(async move {
            loop {
                let (index, root, cfg) = (index.clone(), root.clone(), cfg.clone());
                let res = tokio::task::spawn_blocking(move || {
                    let index = index.lock().unwrap();
                    let ev = upload::evict(
                        &index,
                        &root,
                        budget,
                        |c, s| cfg.policy(c, s),
                        upload::now_ms(),
                    );
                    // Event stills go with their recordings.
                    match argus_server::stills::StillStore::new(&root).expire(&index) {
                        Ok(n) if n > 0 => info!(events = n, "removed old event stills"),
                        Ok(_) => {}
                        Err(e) => warn!("expiring event stills: {e:#}"),
                    }
                    // Detection tracks: kept 30 days (they're small).
                    let month_ago = upload::now_ms() - 30 * 86_400_000;
                    if let Err(e) = index.expire_tracks(month_ago) {
                        warn!("expiring detection tracks: {e:#}");
                    }
                    ev
                })
                .await;
                match res {
                    Ok(Ok(ev)) => {
                        if ev.chunks > 0 {
                            info!(
                                chunks = ev.chunks,
                                mb = ev.bytes / 1_000_000,
                                "freed local disk"
                            );
                        }
                        if ev.over_budget > 0 {
                            warn!(
                                over_mb = ev.over_budget / 1_000_000,
                                "over local_budget_gb: the rest is waiting to upload or may still get an event"
                            );
                        }
                    }
                    Ok(Err(e)) => warn!("eviction failed: {e:#}"),
                    Err(e) => warn!("eviction task failed: {e}"),
                }
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        }));
    }

    tokio::signal::ctrl_c().await?;
    info!("shutting down, closing open chunks");
    for t in background.iter().chain(&ingest) {
        t.abort();
    }
    for s in live.all() {
        s.close(); // ends each recorder once it has written what it has
    }
    for r in recorders {
        r.await??;
    }
    Ok(())
}
