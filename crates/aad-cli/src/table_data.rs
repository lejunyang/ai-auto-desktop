//! Local file boundary for exporting and reconciling a table collection.
use clap::Subcommand;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::io::Read;
use std::path::{Path, PathBuf};

type Result<T> = std::result::Result<T, String>;
type Record = BTreeMap<String, String>;
const MAX_FILE: u64 = 16 * 1024 * 1024;
const MAX_DIFFERENCES: usize = 100;

#[derive(Subcommand)]
pub enum TableCommand {
    /// Traverse one Chromium table, optionally export CSV and reconcile it in this run.
    Collect(CollectArgs),
    #[command(flatten)]
    Csv(CsvCommand),
}

#[derive(Subcommand)]
pub enum CsvCommand {
    /// Write a complete collection to a new UTF-8 CSV file.
    Export {
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    /// Compare a complete collection to an expected CSV, by stable row key.
    Compare {
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        csv: PathBuf,
        /// Compare this column numerically and return both totals. Repeatable.
        #[arg(long, value_name = "COLUMN")]
        decimal: Vec<String>,
    },
}

#[derive(clap::Args)]
#[command(group(clap::ArgGroup::new("page").required(true).args(["resource_id","url"])))]
pub struct CollectArgs {
    /// JSON collection spec containing table, columns, row_key and traversal.
    #[arg(long)]
    spec: PathBuf,
    /// Exact URL of one already open main frame.
    #[arg(long)]
    url: Option<String>,
    /// Exact Chromium resource id (also supports frames).
    #[arg(long)]
    resource_id: Option<String>,
    #[arg(long,default_value_t=9222,value_parser=clap::value_parser!(u16).range(1..))]
    browser_port: u16,
    /// Grant input for the observed pagination buttons or scroll container.
    #[arg(long)]
    allow_input: bool,
    /// New CSV file to create for a complete result.
    #[arg(long)]
    output: Option<PathBuf>,
    /// Expected CSV file to reconcile against.
    #[arg(long)]
    compare_csv: Option<PathBuf>,
    #[arg(long, requires = "compare_csv", value_name = "COLUMN")]
    decimal: Vec<String>,
    /// Collection deadline in milliseconds.
    #[arg(long,default_value_t=30000,value_parser=clap::value_parser!(u64).range(1..=30000))]
    timeout_ms: u64,
}

#[derive(Deserialize)]
struct Dataset {
    complete: bool,
    scope: Option<String>,
    columns: Vec<String>,
    row_key: String,
    row_count: usize,
    expected_rows: Option<usize>,
    records: Vec<Record>,
}

fn read_local(path: &Path) -> Result<Vec<u8>> {
    let metadata = path
        .symlink_metadata()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if !metadata.is_file() || metadata.len() > MAX_FILE {
        return Err("input must be a regular file of at most 16 MiB".into());
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(|e| e.to_string())?;
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err("expected a regular input file".into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_FILE {
        return Err("input exceeded 16 MiB".into());
    }
    Ok(bytes)
}

impl Dataset {
    fn load(path: &Path) -> Result<Self> {
        let bytes = read_local(path)?;
        let data: Self =
            serde_json::from_slice(&bytes).map_err(|e| format!("invalid collection: {e}"))?;
        Self::validated(data)
    }

    fn validated(data: Self) -> Result<Self> {
        if !data.complete {
            return Err("collection is incomplete".into());
        }
        if data.scope.as_deref() == Some("pagination_from_current_page")
            && data.expected_rows.is_none()
        {
            return Err(
                "paginated collection has no first-page witness or expected row total".into(),
            );
        }
        if data.row_count != data.records.len()
            || data.expected_rows.is_some_and(|n| n != data.row_count)
            || data.row_count > 10_000
        {
            return Err("invalid collection row count".into());
        }
        let names: BTreeSet<_> = data.columns.iter().collect();
        if !(1..=32).contains(&names.len())
            || names.len() != data.columns.len()
            || !names.contains(&data.row_key)
            || names.iter().any(|s| s.trim().is_empty() || s.len() > 256)
        {
            return Err("invalid collection columns or row_key".into());
        }
        if data
            .records
            .iter()
            .any(|r| r.len() != names.len() || !r.keys().all(|k| names.contains(k)))
        {
            return Err("every record must contain exactly the selected string columns".into());
        }
        index(&data.records, &data.row_key)?;
        Ok(data)
    }
}

fn index<'a>(rows: &'a [Record], key: &str) -> Result<BTreeMap<&'a str, &'a Record>> {
    let mut keyed = BTreeMap::new();
    for row in rows {
        let id = row
            .get(key)
            .filter(|s| !s.trim().is_empty())
            .ok_or("empty row key")?;
        if keyed.insert(id.as_str(), row).is_some() {
            return Err(format!("duplicate row key {id:?}"));
        }
    }
    Ok(keyed)
}

fn export(data: &Dataset, path: &Path) -> Result<Value> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
    {
        let mut writer = csv::WriterBuilder::new()
            .terminator(csv::Terminator::CRLF)
            .from_writer(tmp.as_file_mut());
        writer
            .write_record(&data.columns)
            .map_err(|e| e.to_string())?;
        for row in &data.records {
            writer
                .write_record(data.columns.iter().map(|c| &row[c]))
                .map_err(|e| e.to_string())?;
        }
        writer.flush().map_err(|e| e.to_string())?;
    }
    tmp.persist_noclobber(path)
        .map_err(|e| format!("cannot create {}: {e}", path.display()))?;
    Ok(json!({"output":path,"rows":data.row_count,"columns":data.columns}))
}

fn read_csv(path: &Path, columns: &[String]) -> Result<Vec<Record>> {
    let bytes = read_local(path)?;
    let mut reader = csv::ReaderBuilder::new().from_reader(bytes.as_slice());
    let header = reader.headers().map_err(|e| e.to_string())?.clone();
    if header.len() != columns.len()
        || header.iter().collect::<BTreeSet<_>>() != columns.iter().map(String::as_str).collect()
    {
        return Err("CSV header must contain each selected column exactly once".into());
    }
    let mut rows = Vec::new();
    for record in reader.records() {
        if rows.len() >= 10_000 {
            return Err("CSV exceeds 10000 rows".into());
        }
        let record = record.map_err(|e| e.to_string())?;
        rows.push(
            header
                .iter()
                .zip(record.iter())
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect(),
        );
    }
    Ok(rows)
}

/// Canonical signed base-ten number with up to 18 fractional places.
#[derive(Default, Clone, Copy, PartialEq, Eq)]
struct Decimal {
    units: i128,
    scale: u32,
}

impl Decimal {
    fn new(mut units: i128, mut scale: u32) -> Self {
        while scale > 0 && units % 10 == 0 {
            units /= 10;
            scale -= 1;
        }
        Self { units, scale }
    }
    fn parse(s: &str) -> Result<Self> {
        let raw = s.strip_prefix(['-', '+']).unwrap_or(s);
        let (whole, fraction) = raw.split_once('.').unwrap_or((raw, ""));
        if whole.is_empty()
            || !whole
                .bytes()
                .chain(fraction.bytes())
                .all(|b| b.is_ascii_digit())
            || fraction.len() > 18
            || (raw.contains('.') && fraction.is_empty())
        {
            return Err(
                "expected a signed base-ten decimal with up to 18 fractional places".into(),
            );
        }
        let digits = format!(
            "{}{whole}{fraction}",
            if s.starts_with('-') { "-" } else { "" }
        );
        let units = digits.parse().map_err(|_| "decimal is out of range")?;
        Ok(Self::new(units, fraction.len() as u32))
    }
    fn add(self, other: Self) -> Result<Self> {
        let scale = self.scale.max(other.scale);
        let align = |d: Self| {
            d.units
                .checked_mul(10i128.pow(scale - d.scale))
                .ok_or("decimal total is out of range")
        };
        Ok(Self::new(
            align(self)?
                .checked_add(align(other)?)
                .ok_or("decimal total is out of range")?,
            scale,
        ))
    }
    fn plain(self) -> String {
        if self.scale == 0 {
            return self.units.to_string();
        }
        let scale = self.scale as usize;
        let digits = format!("{:0width$}", self.units.unsigned_abs(), width = scale + 1);
        let (whole, part) = digits.split_at(digits.len() - scale);
        format!("{}{whole}.{part}", if self.units < 0 { "-" } else { "" })
    }
}

fn total(rows: &[Record], column: &str) -> Result<String> {
    rows.iter()
        .try_fold(Decimal::default(), |sum, r| {
            sum.add(Decimal::parse(&r[column])?)
        })
        .map(Decimal::plain)
        .map_err(|e| format!("column {column:?}: {e}"))
}

fn compare(data: &Dataset, expected_rows: &[Record], decimal: &[String]) -> Result<Value> {
    let numeric: BTreeSet<_> = decimal.iter().collect();
    if numeric.len() != decimal.len()
        || numeric.contains(&data.row_key)
        || decimal.iter().any(|c| !data.columns.contains(c))
    {
        return Err("decimal columns must be distinct selected non-key columns".into());
    }
    let expected = index(expected_rows, &data.row_key)?;
    let actual = index(&data.records, &data.row_key)?;
    let mut totals = BTreeMap::new();
    for column in decimal {
        totals.insert(
            column,
            json!({"actual":total(&data.records,column)?,"expected":total(expected_rows,column)?}),
        );
    }
    let mut differences = Vec::new();
    let mut difference_count = 0;
    let mut report = |difference: Value| {
        difference_count += 1;
        if differences.len() < MAX_DIFFERENCES {
            differences.push(difference);
        }
    };
    for key in actual
        .keys()
        .chain(expected.keys())
        .copied()
        .collect::<BTreeSet<_>>()
    {
        match (actual.get(key), expected.get(key)) {
            (Some(row), None) => report(json!({"kind":"unexpected","key":key,"actual":row})),
            (None, Some(row)) => report(json!({"kind":"missing","key":key,"expected":row})),
            (Some(a), Some(b)) => {
                for column in &data.columns {
                    let equal = if numeric.contains(column) {
                        Decimal::parse(&a[column])? == Decimal::parse(&b[column])?
                    } else {
                        a[column] == b[column]
                    };
                    if !equal {
                        report(
                            json!({"kind":"changed","key":key,"column":column,"actual":a[column],"expected":b[column]}),
                        );
                    }
                }
            }
            _ => unreachable!(),
        }
    }
    Ok(
        json!({"matches":difference_count==0,"row_key":data.row_key,"actual_rows":actual.len(),"expected_rows":expected.len(),
        "difference_count":difference_count,"differences":differences,"differences_truncated":difference_count>differences.len(),"totals":totals}),
    )
}

pub fn collect(args: &CollectArgs) -> Result<Value> {
    use aad_control::{
        browser::{BrowserBackend, PROVIDER},
        table::Collection,
        Operation, Policy, Request, Session,
    };
    if !args.allow_input {
        return Err("table collection requires --allow-input".into());
    }
    let spec: Collection = serde_json::from_slice(&read_local(&args.spec)?)
        .map_err(|e| format!("invalid table spec: {e}"))?;
    let expected = args
        .compare_csv
        .as_deref()
        .map(|file| read_csv(file, &spec.columns))
        .transpose()?;
    if let Some(output) = &args.output {
        match output.symlink_metadata() {
            Ok(_) => return Err(format!("output already exists: {}", output.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    let session = Session::new(
        vec![std::sync::Arc::new(BrowserBackend::new(args.browser_port))],
        Policy {
            allow_input: true,
            ..Default::default()
        },
    );
    let request = |operation| {
        let mut req = Request::new(Some(session.id().into()), operation);
        req.timeout_ms = args.timeout_ms;
        let result = session.execute(&req);
        result
            .data
            .ok_or_else(|| result.error.unwrap_or_default().to_string())
    };
    let listed = request(Operation::Targets {
        provider: Some(PROVIDER.into()),
    })?;
    if let Some(failure) = listed["unavailable"].get(PROVIDER) {
        return Err(format!("Chromium discovery failed: {failure}"));
    }
    let matches: Vec<_> = listed["resources"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| match &args.resource_id {
            Some(id) => r["resource_id"].as_str() == Some(id),
            None => {
                r["metadata"]["main_frame"] == true
                    && r["metadata"]["url"].as_str() == args.url.as_deref()
            }
        })
        .collect();
    let [page] = matches.as_slice() else {
        return Err(format!(
            "expected one Chromium resource, found {}",
            matches.len()
        ));
    };
    let resource_id = page["resource_id"]
        .as_str()
        .ok_or("resource id missing")?
        .to_owned();
    let mut result = request(Operation::CollectTable {
        resource_id,
        spec: Box::new(spec),
    })?;
    if result["complete"] != true || (args.output.is_none() && expected.is_none()) {
        return Ok(result);
    }
    let data = Dataset::validated(
        serde_json::from_value(result.clone())
            .map_err(|e| format!("invalid collection for CSV: {e}"))?,
    )?;
    if let Some(expected) = expected {
        let comparison = compare(&data, &expected, &args.decimal)?;
        result["matches"] = comparison["matches"].clone();
        result["comparison"] = comparison;
    }
    if let Some(path) = &args.output {
        result["export"] = export(&data, path)?;
    }
    Ok(result)
}

pub fn csv(command: &CsvCommand) -> Result<Value> {
    match command {
        CsvCommand::Export { input, output } => export(&Dataset::load(input)?, output),
        CsvCommand::Compare {
            input,
            csv,
            decimal,
        } => {
            let data = Dataset::load(input)?;
            compare(&data, &read_csv(csv, &data.columns)?, decimal)
        }
    }
}

pub fn outcome(result: Result<Value>) -> (Value, u8) {
    use crate::{failure, EXIT_FAILED, EXIT_OK};
    match result {
        Ok(value) if value["complete"] == false || value["matches"] == false => {
            (value, EXIT_FAILED)
        }
        Ok(value) => (value, EXIT_OK),
        Err(message) => (failure("CLI.TABLE_ERROR", &message, None), EXIT_FAILED),
    }
}
