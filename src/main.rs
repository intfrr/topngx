use std::fs::File;
use std::io::{self, BufRead, BufReader};

use anyhow::{anyhow, Result};
use log::{debug, info};
use regex::Regex;
use rusqlite::types::ToSql;
use structopt::StructOpt;

use nginx::{available_variables, format_to_pattern};
use processor::{generate_processor, Processor};

mod nginx;
mod processor;

const STDIN: &str = "STDIN";

// Common field names.
const STATUS_TYPE: &str = "status_type";
const BYTES_SENT: &str = "bytes_sent";
const REQUEST_PATH: &str = "request_path";

#[derive(Debug, StructOpt)]
#[structopt(
    author,
    name = "topngx",
    about = "top for NGINX",
    rename_all = "kebab-case"
)]
struct Options {
    /// The access log to parse.
    #[structopt(short, long)]
    access_log: Option<String>,

    /// The specific log format with which to parse.
    #[structopt(short, long, default_value = "combined")]
    format: String,

    /// Group by this variable.
    #[structopt(short, long, default_value = "request_path")]
    group_by: String,

    /// Having clause.
    #[structopt(short = "w", long, default_value = "1")]
    having: u64,

    /// Refresh the statistics using this interval which is given in seconds.
    #[structopt(short = "t", long, conflicts_with = "no_follow", default_value = "2")]
    interval: u64,

    /// The number of records to limit for each query.
    #[structopt(short, long, default_value = "10")]
    limit: u64,

    /// Do not tail the log file and only report what is currently there.
    #[structopt(short, long)]
    no_follow: bool,

    /// Order of output for the default queries.
    #[structopt(short, long, default_value = "count")]
    order_by: String,

    #[structopt(subcommand)]
    subcommand: Option<SubCommand>,
}

// The list of subcommands available to use.
#[derive(Debug, StructOpt)]
enum SubCommand {
    /// Print the average of the given fields.
    Avg(Fields),

    /// List the available fields as well as the access log and format being used.
    Info,

    /// Print out the supplied fields with the given limit.
    Print(Fields),

    /// Supply a custom query.
    Query(Query),

    /// Compute the sum of the given fields.
    Sum(Fields),

    /// Find the top values for the given fields.
    Top(Fields),
}

#[derive(Debug, StructOpt)]
struct Fields {
    /// A space Separated list of field names.
    fields: Vec<String>,
}

#[derive(Debug, StructOpt)]
struct Query {
    /// A space separated list of field names.
    #[structopt(short, long)]
    fields: Vec<String>,

    /// The supplied query. You typically will want to use your shell to quote it.
    #[structopt(short, long)]
    query: String,
}

// Either read from STDIN or the file specified.
fn input_source(opts: &Options, access_log: &str) -> Result<Box<dyn BufRead>> {
    if access_log == STDIN {
        Ok(Box::new(BufReader::new(io::stdin())))
    } else if opts.no_follow {
        Ok(Box::new(BufReader::new(File::open(access_log)?)))
    } else {
        Err(anyhow!("following log files is not currently implemented"))
    }
}

fn run(opts: &Options, fields: Option<Vec<String>>, queries: Option<Vec<String>>) -> Result<()> {
    let access_log = match &opts.access_log {
        Some(l) => &l,
        None => {
            if atty::isnt(atty::Stream::Stdin) {
                STDIN
            } else {
                return Err(anyhow!("STDIN is a TTY"));
            }
        }
    };
    info!("access log: {}", access_log);
    info!("access log format: {}", opts.format);

    let input = input_source(opts, access_log)?;
    let pattern = format_to_pattern(&opts.format)?;
    let processor = generate_processor(opts, fields, queries)?;
    parse_input(input, &pattern, &processor)?;
    processor.report()
}

fn parse_input(input: Box<dyn BufRead>, pattern: &Regex, processor: &Processor) -> Result<()> {
    let mut records = vec![];

    for line in input.lines() {
        match pattern.captures(&line?) {
            None => {}
            Some(c) => {
                let mut record: Vec<(String, Box<dyn ToSql>)> = vec![];

                for field in &processor.fields {
                    if field == STATUS_TYPE {
                        let status = c.name("status").map_or("", |m| m.as_str());
                        let status_type = status.parse::<u16>().unwrap_or(0) / 100;
                        record.push((format!(":{}", field), Box::new(status_type)));
                    } else if field == BYTES_SENT {
                        let bytes_sent = c.name("body_bytes_sent").map_or("", |m| m.as_str());
                        let bytes_sent = bytes_sent.parse::<u32>().unwrap_or(0);
                        record.push((format!(":{}", field), Box::new(bytes_sent)));
                    } else if field == REQUEST_PATH {
                        if c.name("request_uri").is_some() {
                            record.push((
                                format!(":{}", field),
                                Box::new(c.name("request_uri").unwrap().as_str().to_string()),
                            ));
                        } else {
                            let uri = c.name("request").map_or("", |m| m.as_str());
                            record.push((format!(":{}", field), Box::new(uri.to_string())));
                        }
                    } else {
                        let value = c.name(field).map_or("", |m| m.as_str());
                        record.push((format!(":{}", field), Box::new(String::from(value))));
                    }
                }

                records.push(record);
            }
        }
    }

    processor.process(records)
}

fn avg_subcommand(opts: &Options, fields: Vec<String>) -> Result<()> {
    let avg_fields: Vec<String> = fields.iter().map(|f| format!("AVG({f})", f = f)).collect();
    let selections = avg_fields.join(", ");
    let query = format!("SELECT {selections} FROM log", selections = selections);
    debug!("average sub command query: {}", query);
    run(opts, Some(fields), Some(vec![query]))
}

fn info_subcommand(opts: &Options) -> Result<()> {
    println!(
        "access log file: {}",
        opts.access_log
            .clone()
            .unwrap_or_else(|| String::from(STDIN))
    );
    println!("access log format: {}", opts.format);
    println!(
        "available variables to query: {}",
        available_variables(&opts.format)?
    );

    Ok(())
}

fn print_subcommand(opts: &Options, fields: Vec<String>) -> Result<()> {
    let selections = fields.join(", ");
    let query = format!(
        "SELECT {selections} FROM log GROUP BY {selections}",
        selections = selections
    );
    debug!("print sub command query: {}", query);
    run(opts, Some(fields), Some(vec![query]))
}

fn query_subcommand(opts: &Options, fields: Vec<String>, query: String) -> Result<()> {
    debug!("custom query: {}", query);
    run(opts, Some(fields), Some(vec![query]))
}

fn sum_subcommand(opts: &Options, fields: Vec<String>) -> Result<()> {
    let sum_fields: Vec<String> = fields.iter().map(|f| format!("SUM({f})", f = f)).collect();
    let selections = sum_fields.join(", ");
    let query = format!("SELECT {selections} FROM log", selections = selections);
    debug!("sum sub command query: {}", query);
    run(opts, Some(fields), Some(vec![query]))
}

fn top_subcommand(opts: &Options, fields: Vec<String>) -> Result<()> {
    let mut queries = Vec::with_capacity(fields.len());

    for f in &fields {
        let query = format!(
            "SELECT {field}, COUNT(1) AS count FROM log \
            GROUP BY {field} ORDER BY COUNT DESC LIMIT {limit}",
            field = f,
            limit = opts.limit
        );
        debug!("top sub command query: {}", query);
        queries.push(query);
    }

    run(opts, Some(fields), Some(queries))
}

fn main() -> Result<()> {
    env_logger::init();

    let opts = Options::from_args();
    debug!("options: {:?}", opts);

    if let Some(sc) = &opts.subcommand {
        match sc {
            SubCommand::Avg(f) => avg_subcommand(&opts, f.fields.clone())?,
            SubCommand::Info => info_subcommand(&opts)?,
            SubCommand::Print(f) => print_subcommand(&opts, f.fields.clone())?,
            SubCommand::Query(q) => query_subcommand(&opts, q.fields.clone(), q.query.clone())?,
            SubCommand::Sum(f) => sum_subcommand(&opts, f.fields.clone())?,
            SubCommand::Top(f) => top_subcommand(&opts, f.fields.clone())?,
        }
        return Ok(());
    }

    run(&opts, None, None)
}
