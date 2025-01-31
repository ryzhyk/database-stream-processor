use clap::Parser;
use dataflow_jit::{
    codegen::CodegenConfig,
    dataflow::CompiledDataflow,
    ir::{GraphExt, Validator},
    sql_graph::SqlGraph,
};
use dbsp::Runtime;
use jsonschema::paths::PathChunk;
use serde_json::Value;
use std::{
    fs::File,
    io::{self, Read},
    path::{Path, PathBuf},
    process::ExitCode,
};

fn main() -> ExitCode {
    {
        use tracing_subscriber::{filter::EnvFilter, fmt, prelude::*};

        tracing_subscriber::registry()
            .with(EnvFilter::try_from_env("DATAFLOW_JIT_LOG").unwrap_or_default())
            .with(fmt::layer())
            .init();
    }

    let args = Args::parse();

    let schema_json = {
        let schema = schemars::schema_for!(SqlGraph);
        let schema = serde_json::to_string_pretty(&schema).unwrap();
        if args.print_schema {
            println!("{schema}");
        }

        serde_json::from_str::<Value>(&schema).unwrap()
    };

    let mut source: Box<dyn Read> = if args.file == Path::new("-") {
        Box::new(io::stdin())
    } else {
        if args.file.extension().is_none() {
            eprintln!(
                "warning: {} has no extension and is not a json file",
                args.file.display(),
            );
        } else if let Some(extension) = args.file.extension() {
            if extension != Path::new("json") {
                eprintln!("warning: {} is not a json file", args.file.display());
            }
        }

        match File::open(&args.file) {
            Ok(file) => Box::new(file),
            Err(error) => {
                eprintln!("failed to read {}: {error}", args.file.display());
                return ExitCode::FAILURE;
            }
        }
    };

    let mut raw_source = String::new();
    if let Err(error) = source.read_to_string(&mut raw_source) {
        eprintln!("failed to read input graph: {error}");
        return ExitCode::FAILURE;
    }

    let source: Value = match serde_json::from_str(&raw_source) {
        Ok(source) => source,
        Err(error) => {
            eprintln!("failed to parse json: {error}");
            return ExitCode::FAILURE;
        }
    };

    match jsonschema::JSONSchema::options()
        .with_draft(jsonschema::Draft::Draft7)
        .compile(&schema_json)
    {
        Ok(schema) => {
            if let Err(errors) = schema.validate(&source) {
                let mut total_errors = 0;
                for error in errors {
                    eprintln!(
                        "json validation error at `{}`: {error}",
                        error.instance_path,
                    );

                    // FIXME: Schema paths aren't correct, see
                    // https://github.com/Stranger6667/jsonschema-rs/issues/426
                    let mut expected_schema = &schema_json;
                    for key in error.schema_path.iter() {
                        expected_schema = match key {
                            PathChunk::Property(property) => &expected_schema[&**property],
                            PathChunk::Index(index) => &expected_schema[index],
                            PathChunk::Keyword(keyword) => &expected_schema[keyword],
                        };
                    }

                    if !expected_schema.is_null() {
                        eprintln!("expected item schema: {expected_schema}");
                    }

                    total_errors += 1;
                }

                eprintln!(
                    "encountered {total_errors} error{} while validating json, exiting",
                    if total_errors == 1 { "" } else { "s" },
                );
                return ExitCode::FAILURE;
            }
        }

        Err(error) => eprintln!("failed to compile json schema: {error}"),
    }

    let mut graph = match serde_json::from_value::<SqlGraph>(source) {
        Ok(graph) => graph.rematerialize(),
        Err(error) => {
            eprintln!("failed to parse json from {}: {error}", args.file.display());
            return ExitCode::FAILURE;
        }
    };

    // TODO: Validate the given graph once validation works

    println!("Unoptimized: {graph:#?}");
    if let Err(error) = Validator::new(graph.layout_cache().clone()).validate_graph(&graph) {
        eprintln!("validation error: {error}");
        return ExitCode::FAILURE;
    }
    graph.optimize();

    let (dataflow, jit_handle, _layout_cache) =
        CompiledDataflow::new(&graph, CodegenConfig::release());

    let (runtime, _) =
        Runtime::init_circuit(1, move |circuit| dataflow.construct(circuit)).unwrap();
    if let Err(_error) = runtime.kill() {
        eprintln!("failed to kill runtime");
        return ExitCode::FAILURE;
    }
    unsafe { jit_handle.free_memory() }

    ExitCode::SUCCESS
}

#[derive(Parser)]
struct Args {
    /// The file to parse json from, if `-` is passed then stdin will be read
    /// from
    pub file: PathBuf,
    /// Print the json schema of the dataflow graph
    #[clap(long)]
    pub print_schema: bool,
}
