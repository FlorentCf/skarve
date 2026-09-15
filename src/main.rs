use anyhow::{Result, bail, ensure};
use raster_engine::session::{Session, request};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, BufRead, Read, Write},
    sync::atomic::AtomicBool,
};

const LIMIT: u64 = 64 * 1024 * 1024;
// Ordinary CLI commands parse/transform JSON before the native request preflight.
// Bound that additional staging separately; serve/run retain the raw protocol cap.
const COMMAND_LIMIT: u64 = 4 * 1024 * 1024;
const HELP: &str = "Skarve — native-grid planar fractional raster statistics\n\n\
  skarve inspect SOURCE\n\
  skarve carve SOURCE GEOMETRY.json --crs EPSG:4326 [--bands 0,1] [--metrics sum,mean,min,max] [--backend native|exactextract|auto]\n\
  skarve ward SOURCE INDEX [--tile-edge 64] [--boundary-source original]\n\
  skarve compile SOURCE --output DATA.skv [--chunk-edge 256] [--band-group 4] [--codec deflate] [--predictor none|byte_delta_v1] [--payload-layout band|row_group_v1] [--no-summaries]\n\
  skarve verify-skv SOURCE\n\
  skarve sum-selected SOURCE SELECTIONS.json --numerical-policy hm_demographics_ordered_v1\n\
  skarve sum-selected --profile PROFILE.json SELECTIONS.json --view-id VIEW --numerical-policy hm_demographics_ordered_v1 [--access-class CLASS]\n\
  skarve cleave JOB.json [--max-rows 128]\n\
  skarve backends\n\
  skarve session [COMMANDS.jsonl]\n\
  skarve --version | smoke | serve | run REQUEST.json\n\n\
SOURCE is a local/HTTPS path or @SPEC.json. Band indices are zero based within\n\
the registered reader. GEOMETRY may be a GeoJSON geometry or Feature. Commands\n\
inspect/measure/prepare own and close their source within one process. Session\n\
keeps readers open for JSON-line register/open, inspect, measure, prepare, batch\n\
and close operations. measure/prepare/batch remain compatibility aliases.\n\
Native batches stream with checkpoints; optional exactextract stages a bounded\n\
complete typed job before pages and does not support resume checkpoints. No implicit\n\
reprojection or geodesic weighting. See the installed workflow documentation.";

fn read_json(path: &str) -> Result<Value> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(COMMAND_LIMIT + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= COMMAND_LIMIT,
        "ordinary CLI JSON input exceeds 4 MiB; use the native session API for larger jobs"
    );
    Ok(serde_json::from_slice(&bytes)?)
}
fn native_call(session: &mut Session, value: Value, cancel: &AtomicBool) -> Result<Value> {
    let input = serde_json::to_string(&value)?;
    drop(value);
    let response = request(session, &input, cancel);
    let mut envelope: Value = serde_json::from_str(&response)?;
    ensure!(
        envelope["ok"] == true,
        "{}",
        envelope["error"]
            .as_str()
            .unwrap_or("native request failed")
    );
    Ok(envelope["result"].take())
}
fn emit(value: Value) -> Result<()> {
    let mut out = io::stdout().lock();
    serde_json::to_writer(&mut out, &json!({"ok":true,"result":value}))?;
    writeln!(out)?;
    out.flush()?;
    Ok(())
}
fn spec(source: &str) -> Result<Value> {
    if let Some(path) = source.strip_prefix('@') {
        read_json(path)
    } else {
        Ok(json!({"location":source}))
    }
}
fn batch(
    session: &mut Session,
    document: Value,
    max_rows: usize,
    cancel: &AtomicBool,
) -> Result<()> {
    ensure!(
        (1..=4096).contains(&max_rows),
        "max-rows must be between 1 and 4096"
    );
    let mut job = if let Some(job) = document.get("job") {
        if let Some(sources) = document.get("sources") {
            for (id, source) in sources.as_object().ok_or_else(|| {
                anyhow::anyhow!("sources must be an object of source specifications")
            })? {
                native_call(
                    session,
                    json!({"op":"register_source","id":id,"spec":source}),
                    cancel,
                )?;
            }
        }
        job.clone()
    } else {
        document
    };
    if let Some(metrics) = job.as_object_mut().and_then(|j| j.remove("metrics")) {
        ensure!(
            job["options"].get("statistics").is_none(),
            "metrics and options.statistics are mutually exclusive"
        );
        if job.get("options").is_none() {
            job["options"] = json!({});
        }
        job["options"]["statistics"] = metrics;
    }
    native_call(
        session,
        json!({"op":"start_job","id":"cli","job":job}),
        cancel,
    )?;
    let result = (|| -> Result<()> {
        loop {
            let page = native_call(
                session,
                json!({"op":"next_job","id":"cli","max_rows":max_rows,"include_checkpoint":true}),
                cancel,
            )?;
            let complete = page["complete"] == true;
            emit(page)?;
            if complete {
                return Ok(());
            }
        }
    })();
    let closed = native_call(session, json!({"op":"close_job","id":"cli"}), cancel);
    result?;
    closed?;
    Ok(())
}
fn session_lines(
    reader: &mut dyn BufRead,
    session: &mut Session,
    cancel: &AtomicBool,
    aliases: bool,
) -> Result<()> {
    loop {
        let mut line = String::new();
        let size = (&mut *reader).take(LIMIT + 1).read_line(&mut line)?;
        if size == 0 {
            return Ok(());
        }
        ensure!(size as u64 <= LIMIT, "request exceeds 64 MiB");
        if line.trim().is_empty() {
            continue;
        }
        if !aliases {
            println!("{}", request(session, &line, cancel));
            io::stdout().flush()?;
            continue;
        }
        ensure!(
            size as u64 <= COMMAND_LIMIT,
            "ordinary CLI session command exceeds 4 MiB; use serve for the raw protocol"
        );
        let mut value: Value = serde_json::from_str(&line)?;
        let op = value["op"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("session command requires op"))?;
        if op == "batch" || op == "cleave" {
            let max_rows = value["max_rows"].as_u64().unwrap_or(128) as usize;
            batch(session, value["job"].clone(), max_rows, cancel)?;
            continue;
        }
        let translated = match op {
            "register" | "open" | "infuse" => "register_source",
            "inspect" => "source_info",
            "measure" | "carve" => "measure_source",
            "prepare" | "ward" => "prepare_source",
            "close" => "close_source",
            _ => op,
        }
        .to_owned();
        for (branded, canonical) in [("zone", "geometry"), ("metrics", "statistics")] {
            if let Some(v) = value.as_object_mut().and_then(|v| v.remove(branded)) {
                ensure!(
                    value.get(canonical).is_none(),
                    "conflicting branded and compatibility argument"
                );
                value[canonical] = v;
            }
        }
        value["op"] = translated.into();
        emit(native_call(session, value, cancel)?)?;
    }
}
fn options(args: &[String]) -> Result<(Vec<String>, BTreeMap<String, String>)> {
    let mut positional = Vec::new();
    let mut flags = BTreeMap::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg.starts_with("--") {
            if arg == "--no-summaries" {
                ensure!(
                    flags.insert(arg.clone(), "true".into()).is_none(),
                    "duplicate option {arg}"
                );
                continue;
            }
            let value = iter
                .next()
                .ok_or_else(|| anyhow::anyhow!("{arg} requires a value"))?;
            ensure!(!value.starts_with("--"), "{arg} requires a value");
            ensure!(
                flags.insert(arg.clone(), value.clone()).is_none(),
                "duplicate option {arg}"
            );
        } else {
            positional.push(arg.clone());
        }
    }
    Ok((positional, flags))
}
fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match args.first().map(String::as_str).unwrap_or("help") {
        "carve" => "measure",
        "ward" => "prepare",
        "cleave" => "batch",
        "infuse" => "open",
        other => other,
    };
    if matches!(command, "help" | "--help" | "-h") {
        println!("{HELP}");
        return Ok(());
    }
    if matches!(command, "--version" | "version") {
        println!(
            "skarve {} (JSON protocol v1; native-grid planar)",
            env!("CARGO_PKG_VERSION")
        );
        return Ok(());
    }
    let mut session = Session::default();
    let cancel = AtomicBool::new(false);
    match command {
        "backends" => {
            ensure!(args.len() == 1, "backends accepts no arguments");
            emit(native_call(
                &mut session,
                json!({"op":"backends"}),
                &cancel,
            )?)?;
        }
        "serve" | "session" => {
            ensure!(
                args.len() <= if command == "serve" { 1 } else { 2 },
                "session accepts at most one JSON-lines file"
            );
            if let Some(path) = args.get(1) {
                session_lines(
                    &mut io::BufReader::new(File::open(path)?),
                    &mut session,
                    &cancel,
                    true,
                )?;
            } else {
                session_lines(
                    &mut io::stdin().lock(),
                    &mut session,
                    &cancel,
                    command == "session",
                )?;
            }
        }
        "run" => {
            ensure!(args.len() == 2, "run requires one JSON request file");
            let mut input = String::new();
            File::open(&args[1])?
                .take(LIMIT + 1)
                .read_to_string(&mut input)?;
            ensure!(input.len() as u64 <= LIMIT, "JSON input exceeds 64 MiB");
            let response = request(&mut session, &input, &cancel);
            println!("{response}");
            if serde_json::from_str::<Value>(&response)?["ok"] != true {
                std::process::exit(1);
            }
        }
        "smoke" => {
            native_call(
                &mut session,
                json!({"op":"open","id":"r","raster":{"grid":{"width":2,"height":2,"transform":[0,1,0,2,0,-1],"crs":"LOCAL"},"bands":[{"values":[1,-2,3,null]}]}}),
                &cancel,
            )?;
            let result = native_call(
                &mut session,
                json!({"op":"measure","source":"r","crs":"LOCAL","geometry":{"type":"Polygon","coordinates":[[[0,0],[2,0],[2,2],[0,2],[0,0]]]}}),
                &cancel,
            )?;
            ensure!(
                result["bands"][0]["fractional_sum"] == 2.,
                "smoke sum differs"
            );
            emit(result)?;
        }
        "sum-selected" => {
            let (positional, mut flags) = options(&args[1..])?;
            let policy = flags.remove("--numerical-policy").ok_or_else(|| {
                anyhow::anyhow!("sum-selected requires explicit --numerical-policy")
            })?;
            ensure!(
                policy == "hm_demographics_ordered_v1",
                "sum-selected accepts only the explicit hm_demographics_ordered_v1 policy"
            );
            if let Some(profile_path) = flags.remove("--profile") {
                ensure!(
                    positional.len() == 1,
                    "profile sum-selected requires SELECTIONS.json"
                );
                let view_id = flags.remove("--view-id").ok_or_else(|| {
                    anyhow::anyhow!("profile sum-selected requires explicit --view-id")
                })?;
                let access_class = flags
                    .remove("--access-class")
                    .unwrap_or_else(|| "unknown".into());
                ensure!(flags.is_empty(), "unknown profile sum-selected option");
                let result = native_call(
                    &mut session,
                    json!({
                        "op":"measure_ordered_profile", "profile":read_json(&profile_path)?,
                        "request":read_json(&positional[0])?, "numerical_policy":policy,
                        "view_id":view_id, "access_class":access_class
                    }),
                    &cancel,
                )?;
                emit(result)?;
                return Ok(());
            }
            ensure!(
                positional.len() == 2 && flags.is_empty(),
                "sum-selected requires SOURCE SELECTIONS.json and no unknown options"
            );
            let selections = read_json(&positional[1])?;
            native_call(
                &mut session,
                json!({"op":"register_source","id":"cli","spec":spec(&positional[0])?}),
                &cancel,
            )?;
            let result = native_call(
                &mut session,
                json!({"op":"measure_ordered_source","source":"cli","request":selections,"numerical_policy":policy}),
                &cancel,
            );
            let closed = native_call(
                &mut session,
                json!({"op":"close_source","source":"cli"}),
                &cancel,
            );
            emit(result?)?;
            closed?;
        }
        "compile" | "verify-skv" => {
            let (positional, mut flags) = options(&args[1..])?;
            ensure!(
                positional.len() == 1,
                "{command} requires one source; see skarve --help"
            );
            let mut source_spec = spec(&positional[0])?;
            if command == "verify-skv" {
                ensure!(flags.is_empty(), "unknown verify-skv option");
                if source_spec.get("format").is_none() {
                    source_spec["format"] = "skv".into();
                }
                return emit(native_call(
                    &mut session,
                    json!({"op":"verify_skv","spec":source_spec}),
                    &cancel,
                )?);
            }
            let output = flags
                .remove("--output")
                .ok_or_else(|| anyhow::anyhow!("compile requires --output"))?;
            let mut compile_options = json!({});
            for (flag, field) in [
                ("--chunk-edge", "chunk_edge"),
                ("--band-group", "band_group"),
                ("--compression-level", "compression_level"),
                ("--working-bytes", "working_bytes"),
                ("--max-output-bytes", "max_output_bytes"),
            ] {
                if let Some(value) = flags.remove(flag) {
                    compile_options[field] = value.parse::<u64>()?.into();
                }
            }
            if let Some(value) = flags.remove("--codec") {
                compile_options["codec"] = value.into();
            }
            if let Some(value) = flags.remove("--predictor") {
                compile_options["predictor"] = value.into();
            }
            if let Some(value) = flags.remove("--payload-layout") {
                compile_options["payload_layout"] = value.into();
            }
            if flags.remove("--no-summaries").is_some() {
                compile_options["summaries"] = false.into();
            }
            if let Some(value) = flags.remove("--bands") {
                ensure!(
                    source_spec.get("bands").is_none(),
                    "--bands conflicts with source specification bands"
                );
                source_spec["bands"] = serde_json::to_value(
                    value
                        .split(',')
                        .map(str::parse::<usize>)
                        .collect::<std::result::Result<Vec<_>, _>>()?,
                )?;
            }
            ensure!(flags.is_empty(), "unknown compile option");
            native_call(
                &mut session,
                json!({"op":"register_source","id":"cli","spec":source_spec}),
                &cancel,
            )?;
            let result = native_call(
                &mut session,
                json!({"op":"compile_source","source":"cli","output":output,"options":compile_options}),
                &cancel,
            );
            let closed = native_call(
                &mut session,
                json!({"op":"close_source","source":"cli"}),
                &cancel,
            );
            emit(result?)?;
            closed?;
        }
        "inspect" | "measure" | "prepare" | "batch" | "open" | "register" => {
            let (positional, mut flags) = options(&args[1..])?;
            if command == "batch" {
                ensure!(positional.len() == 1, "batch requires JOB.json");
                let max_rows = flags
                    .remove("--max-rows")
                    .unwrap_or_else(|| "128".into())
                    .parse()?;
                ensure!(flags.is_empty(), "unknown batch option");
                return batch(&mut session, read_json(&positional[0])?, max_rows, &cancel);
            }
            let count = if matches!(command, "measure" | "prepare") {
                2
            } else {
                1
            };
            ensure!(
                positional.len() == count,
                "{command} requires {count} positional argument(s); see skarve --help"
            );
            let mut query = json!({"source":"cli"});
            match command {
                "measure" => {
                    let geometry = read_json(&positional[1])?;
                    query["op"] = "measure_source".into();
                    query["geometry"] = if geometry["type"] == "Feature" {
                        geometry["geometry"].clone()
                    } else {
                        geometry
                    };
                    query["crs"] = flags
                        .remove("--crs")
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "measure requires --crs; CRS is never inferred or reprojected"
                            )
                        })?
                        .into();
                    if let Some(metrics) = flags.remove("--metrics") {
                        ensure!(
                            !flags.contains_key("--statistics"),
                            "--metrics and --statistics are mutually exclusive"
                        );
                        flags.insert("--statistics".into(), metrics);
                    }
                    for (flag, field) in [("--bands", "bands"), ("--statistics", "statistics")] {
                        if let Some(value) = flags.remove(flag) {
                            query[field] = if field == "bands" {
                                serde_json::to_value(
                                    value
                                        .split(',')
                                        .map(str::parse::<usize>)
                                        .collect::<std::result::Result<Vec<_>, _>>()?,
                                )?
                            } else {
                                json!(value.split(',').collect::<Vec<_>>())
                            };
                        }
                    }
                    if let Some(index) = flags.remove("--index") {
                        query["index"] = index.into();
                    }
                    for (flag, field) in [
                        ("--backend", "backend"),
                        ("--numerical-policy", "numerical_policy"),
                        ("--execution-envelope", "execution_envelope"),
                    ] {
                        if let Some(value) = flags.remove(flag) {
                            query[field] = json!(value);
                        }
                    }
                    if let Some(value) = flags.remove("--accepted-policies") {
                        query["accepted_policies"] = json!(value.split(',').collect::<Vec<_>>());
                    }
                    if let Some(value) = flags.remove("--backend-options") {
                        query["backend_options"] = serde_json::from_str(&value)?;
                    }
                }
                "prepare" => {
                    query["op"] = "prepare_source".into();
                    query["index"] = positional[1].clone().into();
                    query["tile_edge"] = flags
                        .remove("--tile-edge")
                        .unwrap_or_else(|| "64".into())
                        .parse::<usize>()?
                        .into();
                    query["boundary_source"] = flags
                        .remove("--boundary-source")
                        .unwrap_or_else(|| "original".into())
                        .into();
                }
                _ => {
                    query["op"] = "source_info".into();
                }
            }
            ensure!(
                flags.is_empty(),
                "unknown option(s): {:?}",
                flags.keys().collect::<Vec<_>>()
            );
            native_call(
                &mut session,
                json!({"op":"register_source","id":"cli","spec":spec(&positional[0])?}),
                &cancel,
            )?;
            let result = native_call(&mut session, query, &cancel);
            let closed = native_call(
                &mut session,
                json!({"op":"close_source","source":"cli"}),
                &cancel,
            );
            emit(result?)?;
            closed?;
        }
        _ => bail!("unknown command {command}; run skarve --help"),
    }
    Ok(())
}
