use crate::cmd::stats::{FieldType, Stats};
use crate::config::{Config, Delimiter};
use crate::select::SelectColumns;
use crate::util;
use crate::CliError;
use crate::CliResult;
use anyhow::{anyhow, Result};
use csv::{ByteRecord};
use log::{debug, error, info, warn};
use serde::Deserialize;
use serde_json::{json, value::Number, Map, Value};
use stats::Frequencies;
use std::{fs::File, io::Write, ops::Add, path::Path, collections::hash_map::HashMap};

macro_rules! fail {
    ($mesg:expr) => {
        return Err(CliError::Other($mesg));
    };
}

static USAGE: &str = "
Generate JSON Schema from CSV data.

This command generates reference JSON Schema (Draft 7) from CSV data, 
including simple validation rules based on data statistics.
Please verify and adjust the Type, Value Constraints, and Required Fields as appropriate.

Example output file from `mydata.csv`. If piped from stdin, then filename is `stdin.csv`.

* mydata.csv.schema.json

Usage:
    qsv schema [options] [<input>]

Schema options:
    --value-constraints        Add value constraints based on CSV data (enum, min, max, minLength, MaxLength)
    --enum-threshold NUM       Cardinality threshold for adding enum constraints [default: 12]
    --pattern-columns <args>   Select columns to add pattern constraints [default: none]

Common options:
    -h, --help                 Display this message
    -n, --no-headers           When set, the first row will not be interpreted
                               as headers. Namely, it will be sorted with the rest
                               of the rows. Otherwise, the first row will always
                               appear as the header row in the output.
    -d, --delimiter <arg>      The field delimiter for reading CSV data.
                               Must be a single character. [default: ,]
";

#[derive(Deserialize, Debug)]
struct Args {
    flag_value_constraints: bool,
    flag_enum_threshold: usize,
    #[allow(dead_code)]
    flag_pattern_columns: SelectColumns,
    flag_no_headers: bool,
    flag_delimiter: Option<Delimiter>,
    arg_input: Option<String>,
}

pub fn run(argv: &[&str]) -> CliResult<()> {
    let args: Args = util::get_args(USAGE, argv)?;

    // dbg!(&args);

    let input_path = match &args.arg_input {
        Some(path) => path,
        None => "stdin.csv",
    };
    let input_filename: &str = match &args.arg_input {
        Some(path) => Path::new(path).file_name().unwrap().to_str().unwrap(),
        None => "stdin.csv",
    };

    let schema_output_filename = input_path.to_owned() + ".schema.json";
    let mut schema_output_file =
        File::create(&schema_output_filename).expect("unable to create schema output file");

    let properties_map: Map<String, Value> = if args.flag_value_constraints {
        match infer_schema_from_stats(&args, input_filename) {
            Ok(map) => map,
            Err(e) => {
                let msg = format!("Failed to infer schema via stats and frequency: {e}");
                fail!(msg);
            }
        }
    } else {
        match infer_schema_simple_frequency(&args, input_filename) {
            Ok(map) => map,
            Err(e) => {
                let msg = format!("Failed to infer schema via simple frequency: {e}");
                error!("{msg}");
                fail!(msg);
            }
        }
    };

    let mut fields: Vec<Value> = Vec::new();
    for key in properties_map.keys() {
        fields.push(Value::String(key.clone()));
    }

    // create final JSON object for output
    let schema = json!({
        "$schema": "https://json-schema.org/draft-07/schema",
        "title": format!("JSON Schema for {input_filename}"),
        "description": "Inferred JSON Schema from QSV schema command",
        "type": "object",
        "properties": Value::Object(properties_map),
        "required": Value::Array(fields)
    });

    let schema_pretty = serde_json::to_string_pretty(&schema).expect("prettify schema json");

    schema_output_file
        .write_all(schema_pretty.as_bytes())
        .expect("unable to write schema file");

    // flush error report; file gets closed automagically when out-of-scope
    schema_output_file.flush().unwrap();

    println!("Schema written to {schema_output_filename}");

    Ok(())
}

/// get stats records from cmd::stats
/// returns tuple (csv_fields, csv_stats, stats_col_index_map)
fn get_stats_records(args: &Args) -> 
    CliResult<(ByteRecord, Vec<Stats>, HashMap<String, usize>)> 
{

    let stats_args = crate::cmd::stats::Args {
        arg_input: args.arg_input.clone(),
        flag_select: crate::select::SelectColumns::parse("").unwrap(),
        flag_everything: false,
        flag_mode: false,
        flag_cardinality: true,
        flag_median: false,
        flag_quartiles: false,
        flag_nulls: false,
        flag_nullcount: true,
        flag_jobs: 3,
        flag_output: None,
        flag_no_headers: args.flag_no_headers,
        flag_delimiter: args.flag_delimiter,
    };

    let (csv_fields, csv_stats) = match stats_args.rconfig().indexed() {
        Ok(o) => match o {
            None => {
                info!("no index, triggering sequential stats");
                stats_args.sequential_stats()
            }
            Some(idx) => {
                info!("has index, triggering parallel stats");
                stats_args.parallel_stats(idx)
            }
        },
        Err(e) => {
            warn!("error determining if indexed, triggering sequential stats: {e}");
            stats_args.sequential_stats()
        }
    }?;

    let stats_columns = stats_args.stat_headers();
    debug!("stats columns: {stats_columns:?}");

    let mut stats_col_index_map = HashMap::new();

    for (i, col) in stats_columns.iter().enumerate() {

        if col != "field" {
            // need offset by 1 due to extra "field" column in headers that's not in stats records
            stats_col_index_map.insert(col.to_owned(), i-1);
        }
    }

    Ok((csv_fields, csv_stats, stats_col_index_map))
}

/// get frequency tables from cmd::stats
/// returns tuple (csv_fields, csv_stats, stats_col_index_map)
fn get_frequency_tables(args: &Args, columns: Vec<usize>) -> 
    CliResult<(ByteRecord, Vec<Frequencies<Vec<u8>>>)> 
{
    use itertools::Itertools;
    let columns_str: String = columns.iter().map(ToString::to_string).join(",");

    let freq_args = crate::cmd::frequency::Args {
        arg_input: args.arg_input.clone(),
        flag_select: crate::select::SelectColumns::parse(&columns_str).unwrap(),
        flag_limit: args.flag_enum_threshold,
        flag_asc: false,
        flag_no_nulls: true,
        flag_jobs: 3,
        flag_output: None,
        flag_no_headers: args.flag_no_headers,
        flag_delimiter: args.flag_delimiter,
    };

    let (headers, ftables) = match freq_args.rconfig().indexed()? {
        Some(ref mut idx) => freq_args.parallel_ftables(idx),
        _ => freq_args.sequential_ftables(),
    }?;

    Ok((headers, ftables))
}

fn infer_schema_from_stats(args: &Args, input_filename: &str) -> CliResult<Map<String, Value>> {

    // invoke cmd::stats
    let (csv_fields, csv_stats, stats_col_index_map) = get_stats_records(args)?;

    // track low cardinality columns to invoke cmd::frequency with
    let mut low_cardinality_column_indices = Vec::new();

    // identify low cardinality columns
    for i in 0..csv_fields.len() {
        // grab stats record for current column
        let stats_record = csv_stats.get(i).unwrap().clone().to_record();

        // get Cardinality 
        let col_cardinality = match stats_record.get(stats_col_index_map["cardinality"]) {
            Some(s) => s.parse::<usize>().unwrap_or(0_usize),
            None => 0_usize,
        };
        // debug!("column_{i}: cardinality={col_cardinality}");

        if col_cardinality <= args.flag_enum_threshold {
            // column selector uses 1-based index
            low_cardinality_column_indices.push(i+1);
        };
    }

    debug!("low cardinality columns: {low_cardinality_column_indices:?}");

    // invoke cmd::frequency
    let (freq_csv_fields, frequency_tables) = get_frequency_tables(args, low_cardinality_column_indices)?;

    let mut unique_values_map: HashMap<String, Vec<String>> = HashMap::new();

    // iterate through fields and gather unique values for each field
    for (i, header) in freq_csv_fields.iter().enumerate() {
        let mut unique_values = Vec::new();

        for (val_byte_vec, _count) in frequency_tables[i].most_frequent() {
            match std::str::from_utf8(val_byte_vec) {
                Ok(s) => {
                    unique_values.push(s.to_string());
                },
                Err(e) => {
                    let msg = format!("Can't read value from column {i} as utf8: {e}");
                    error!("{msg}");
                    fail!(msg);
                }
            };
        }

        // convert csv header to string
        let header_string: String = match std::str::from_utf8(header) {
            Ok(s) => s.to_string(),
            Err(e) => {
                let msg = format!("Can't read header from column {i} as utf8: {e}");
                error!("{msg}");
                fail!(msg);
            }
        };

        // sort the values so enum list so schema can be diff'ed between runs
        unique_values.sort();

        debug!("enum[{header_string}]: len={}, val={:?}", unique_values.len(), unique_values);
        unique_values_map.insert(header_string, unique_values);

    }

    // dbg!(&unique_values_map);

    // map holds "properties" object of json schema
    let mut properties_map: Map<String, Value> = Map::new();

    // generate definition for each CSV column/field and add to properties_map
    for i in 0..csv_fields.len() {
        let header = csv_fields.get(i).unwrap();
        // convert csv header to string
        let header_string: String = match std::str::from_utf8(header) {
            Ok(s) => s.to_string(),
            Err(e) => {
                fail!(format!("Can't read header from column {i} as utf8: {e}"));
            }
        };

        // grab stats record for current column
        let stats_record = csv_stats.get(i).unwrap().clone().to_record();

        debug!("stats[{header_string}]: {stats_record:?}");

        // get Type from stats record
        let col_type = stats_record.get(stats_col_index_map["type"]).unwrap();
        // get NullCount 
        let col_null_count = match stats_record.get(stats_col_index_map["nullcount"]) {
            Some(s) => s.parse::<usize>().unwrap_or(0_usize),
            None => 0_usize,
        };


        // debug!(
        //     "{header_string}: type={col_type}, optional={}",
        //     col_null_count > 0
        // );

        // map for holding field definition
        let mut field_map: Map<String, Value> = Map::new();
        let desc = format!("{header_string} column from {input_filename}");
        field_map.insert("description".to_string(), Value::String(desc));

        // use list to hold types, since optional fields get appended a "null" type
        let mut type_list: Vec<Value> = Vec::new();
        let mut enum_list: Vec<Value> = Vec::new();

        match col_type {
            "String" => {
                type_list.push(Value::String("string".to_string()));

                // minLength constraint
                if let Some(min_length_str) = stats_record.get(stats_col_index_map["min_length"]) {
                    let min_length = min_length_str.parse::<u32>().unwrap();
                    field_map.insert(
                        "minLength".to_string(),
                        Value::Number(Number::from(min_length)),
                    );
                };

                // maxLength constraint
                if let Some(max_length_str) = stats_record.get(stats_col_index_map["max_length"]) {
                    let max_length = max_length_str.parse::<u32>().unwrap();
                    field_map.insert(
                        "maxLength".to_string(),
                        Value::Number(Number::from(max_length)),
                    );
                };

                // enum constraint
                if let Some(values) = unique_values_map.get(&header_string) {
                    for value in values {
                        enum_list.push(Value::String(value.to_string()));
                    }
                }
            }
            "Date" => {
                type_list.push(Value::String("string".to_string()));
            }
            "Integer" => {
                type_list.push(Value::String("integer".to_string()));

                if let Some(min_str) = stats_record.get(stats_col_index_map["min"]) {
                    let min = min_str.parse::<i64>().unwrap();
                    field_map.insert("minimum".to_string(), Value::Number(Number::from(min)));
                };

                if let Some(max_str) = stats_record.get(stats_col_index_map["max"]) {
                    let max = max_str.parse::<i64>().unwrap();
                    field_map.insert("maximum".to_string(), Value::Number(Number::from(max)));
                };
            }
            "Float" => {
                type_list.push(Value::String("number".to_string()));

                if let Some(min_str) = stats_record.get(stats_col_index_map["min"]) {
                    let min = min_str.parse::<f64>().unwrap();
                    field_map.insert(
                        "minimum".to_string(),
                        Value::Number(Number::from_f64(min).unwrap()),
                    );
                };

                if let Some(max_str) = stats_record.get(stats_col_index_map["max"]) {
                    let max = max_str.parse::<f64>().unwrap();
                    field_map.insert(
                        "maximum".to_string(),
                        Value::Number(Number::from_f64(max).unwrap()),
                    );
                };
            }
            "NULL" => {
                type_list.push(Value::String("null".to_string()));
            }
            _ => {
                warn!("Stats gave unexpected field type '{col_type}', default to JSON String.");
                // defaults to JSON String
                type_list.push(Value::String("string".to_string()));
            }
        }



        if col_null_count > 0 && !type_list.contains(&Value::String("null".to_string())) {
            // for fields that are not mandatory:
            // JSON String "null" is required in Type list
            type_list.push(Value::String("null".to_string()));
            // and JSON NULL is required in Enum list
            enum_list.push(Value::Null);
        }

        if type_list.len() > 0 {
            field_map.insert("type".to_string(), Value::Array(type_list));
        }

        if enum_list.len() > 0 {
            field_map.insert("enum".to_string(), Value::Array(enum_list));
        }

        // add current field definition to properties map
        properties_map.insert(header_string, Value::Object(field_map));
    }

    Ok(properties_map)
}

fn infer_schema_simple_frequency(args: &Args, input_filename: &str) -> Result<Map<String, Value>> {
    let rconfig = Config::new(&args.arg_input)
        .delimiter(args.flag_delimiter)
        .no_headers(args.flag_no_headers);

    let mut rdr = rconfig.reader()?;
    let headers = rdr.byte_headers()?.clone();

    // amortize memory allocation by reusing record
    #[allow(unused_assignments)]
    let mut record = ByteRecord::new();

    let mut row_index: u32 = 0;

    // array of frequency tables to track non-NULL type occurrences
    let mut frequency_tables: Vec<_> = (0..(headers.len() as u32))
        .map(|_| Frequencies::<FieldType>::new())
        .collect();
    // array of boolean to track if column is NULLABLE
    let mut nullable_flags: Vec<bool> = vec![false; headers.len()];

    // iterate over each CSV column/field and determine type
    let headers_iter = headers.iter().enumerate();

    while rdr.read_byte_record(&mut record)? {
        row_index = row_index.add(1);

        // dbg!(&record);

        for col_index in 0..headers.len() {
            // since from_sample() parses byte slice to string, no need to do it here
            let value_slice: &[u8] = &record[col_index];

            let inferred_type: FieldType = FieldType::from_sample(value_slice);

            // debug!("column_{col_index}[{row_index}]: val={value_slice:?}, type={inferred_type}");

            // update frequency table for this column
            match inferred_type {
                FieldType::TNull => {
                    // only count NULL once, so it won't dominate frequency table when value is optional
                    if !nullable_flags[col_index] {
                        frequency_tables[col_index].add(FieldType::TNull);
                    }
                    nullable_flags[col_index] = true;
                }
                FieldType::TUnknown => {
                    // default to String
                    frequency_tables[col_index].add(FieldType::TString);
                }
                x => {
                    frequency_tables[col_index].add(x);
                }
            }
        }
    } // end main while loop over csv records

    debug!("freq tables: {frequency_tables:?}");

    // map holds "properties" object of json schema
    let mut properties_map: Map<String, Value> = Map::new();

    // iterate through headers again and get most frequent type for each column
    for (i, header) in headers_iter {
        let most_frequent = frequency_tables[i].most_frequent();
        let (inferred_type, _count) = match most_frequent.get(0) {
            Some(tuple) => *tuple,
            None => {
                // not good, no type info for this column
                return Err(anyhow!("Cannot determine type for column '{header:?}'"));
            }
        };

        // convert csv header to string
        let header_string: String = match std::str::from_utf8(header) {
            Ok(s) => s.to_string(),
            Err(e) => {
                return Err(anyhow!("Can't read header from column {i} as utf8: {e}"));
            }
        };

        debug!(
            "{header_string} has most frequent type of {inferred_type:?}, optional={}",
            nullable_flags[i]
        );

        // use list since optional columns get appended a "null" type
        let mut type_list: Vec<Value> = Vec::new();

        match inferred_type {
            FieldType::TString => {
                type_list.push(Value::String("string".to_string()));
            }
            FieldType::TDate => {
                type_list.push(Value::String("string".to_string()));
            }
            FieldType::TInteger => {
                type_list.push(Value::String("integer".to_string()));
            }
            FieldType::TFloat => {
                type_list.push(Value::String("number".to_string()));
            }
            FieldType::TNull => {
                type_list.push(Value::String("null".to_string()));
            }
            _ => {
                // defaults to JSON String
                type_list.push(Value::String("string".to_string()));
            }
        }

        // "null" type denotes optinal value
        // to be compatible with "validate" command, has to come after the real type, and only once
        if nullable_flags[i] && !type_list.contains(&Value::String("null".to_string())) {
            type_list.push(Value::String("null".to_string()));
        }

        let mut field_map: Map<String, Value> = Map::new();
        field_map.insert("type".to_string(), Value::Array(type_list));
        let desc = format!("{header_string} column from {input_filename}");
        field_map.insert("description".to_string(), Value::String(desc));
        properties_map.insert(header_string, Value::Object(field_map));
    } // end for loop over all columns

    Ok(properties_map)
}
