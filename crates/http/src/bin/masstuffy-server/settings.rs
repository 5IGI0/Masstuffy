//! Configuration of `masstuffy-server`: one option table drives the CLI flags, the `MASSTUFFY_*`
//! environment variables, `--help` and the table of `docs/http-api.md`.
//!
//! Precedence: command-line flag > environment variable > built-in default. The process
//! environment is never read here: `main` passes a snapshot to [`parse`].

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use masstuffy::RAW_IMPORT_CHECK_ENV;
use masstuffy_http::{HttpOptions, OptionsError, ServeOptions};

/// Prefix of every environment variable of the server.
pub(crate) const ENV_PREFIX: &str = "MASSTUFFY_";
/// Environment variable of the positional `WORKDIR`.
pub(crate) const WORKDIR_ENV: &str = "MASSTUFFY_WORKDIR";
/// One-line usage for error messages.
pub(crate) const USAGE: &str = "usage: masstuffy-server [WORKDIR] [OPTIONS]   (see --help)";

const KIB: u64 = 1 << 10;
const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;
const TIB: u64 = 1 << 40;

/// Effective server settings (everything but the workdir). `Default` = built-in defaults.
#[derive(Debug, Clone)]
pub(crate) struct Settings {
    pub(crate) listen: SocketAddr,
    pub(crate) init: bool,
    pub(crate) log: tracing::Level,
    pub(crate) http: HttpOptions,
    pub(crate) serve: ServeOptions,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            listen: SocketAddr::from(([127, 0, 0, 1], 8470)),
            init: false,
            log: tracing::Level::INFO,
            http: HttpOptions::default(),
            serve: ServeOptions::default(),
        }
    }
}

/// Where an option's effective value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    Default,
    Env,
    Cli,
}

impl Source {
    fn rank(self) -> u8 {
        match self {
            Source::Default => 0,
            Source::Env => 1,
            Source::Cli => 2,
        }
    }
}

/// How an option takes its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// CLI: bare flag, no value allowed (`--init`); `apply` receives "true".
    /// Env: the value, parsed as a boolean by `apply`.
    Switch,
    /// CLI: `--flag VALUE` or `--flag=VALUE`. Env: the value.
    Value,
}

/// One option. Invariants (checked by tests): flags are unique, start with "--" and contain only
/// `[a-z0-9-]`; derived env names are unique and differ from `WORKDIR_ENV` and
/// `RAW_IMPORT_CHECK_ENV`; `value` is empty iff `kind == Kind::Switch`; `value` and `help`
/// contain no '|' and no newline.
pub(crate) struct Opt {
    /// The CLI flag, e.g. "--max-body-bytes". The env var name is derived from it.
    pub(crate) flag: &'static str,
    /// Value placeholder for help and docs; empty for a switch.
    pub(crate) value: &'static str,
    pub(crate) kind: Kind,
    /// One sentence, ends with '.'.
    pub(crate) help: &'static str,
    /// `OptionsError` field names this option sets (empty for listen/init/log).
    pub(crate) fields: &'static [&'static str],
    /// Parses a value and stores it. `Err` is a message without the flag or variable prefix.
    pub(crate) apply: fn(&mut Settings, &str) -> Result<(), String>,
    /// Renders the option's value (default column, debug log).
    pub(crate) show: fn(&Settings) -> String,
}

/// Result of a successful parse.
pub(crate) struct Args {
    pub(crate) workdir: PathBuf,
    pub(crate) settings: Settings,
    /// Source of each `OPTIONS` entry (same index).
    pub(crate) sources: Vec<Source>,
    /// Non-fatal messages to log once tracing is up.
    pub(crate) warnings: Vec<String>,
}

pub(crate) enum Command {
    Run(Box<Args>),
    Help,
    Version,
}

// ---------------------------------------------------------------- value parsers

enum Bad {
    Syntax,
    TooLarge,
}

/// A non-empty run of ASCII digits as `u64`.
fn digits(s: &str) -> Result<u64, Bad> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Bad::Syntax);
    }
    s.parse::<u64>().map_err(|_| Bad::TooLarge)
}

/// `digits` followed by one of `units` (longest-ending suffixes first, `""` last), scaled.
fn scaled(v: &str, units: &[(&str, u64)]) -> Result<u64, Bad> {
    for (suffix, mult) in units {
        if let Some(rest) = v.strip_suffix(suffix) {
            match digits(rest) {
                Ok(n) => return n.checked_mul(*mult).ok_or(Bad::TooLarge),
                Err(Bad::TooLarge) => return Err(Bad::TooLarge),
                Err(Bad::Syntax) => {}
            }
        }
    }
    Err(Bad::Syntax)
}

fn too_large() -> String {
    "value too large".to_owned()
}

pub(crate) fn parse_count(v: &str) -> Result<usize, String> {
    let bad = || format!("expected a positive integer, got {v:?}");
    match digits(v) {
        Ok(0) | Err(Bad::Syntax) => Err(bad()),
        Err(Bad::TooLarge) => Err(too_large()),
        Ok(n) => usize::try_from(n).map_err(|_| too_large()),
    }
}

pub(crate) fn parse_size(v: &str) -> Result<u64, String> {
    let units = [
        ("KiB", KIB),
        ("MiB", MIB),
        ("GiB", GIB),
        ("TiB", TIB),
        ("", 1),
    ];
    match scaled(v, &units) {
        Ok(0) | Err(Bad::Syntax) => Err(format!(
            "expected a positive size (bytes, or with suffix KiB, MiB, GiB or TiB), got {v:?}"
        )),
        Err(Bad::TooLarge) => Err(too_large()),
        Ok(n) => Ok(n),
    }
}

fn parse_size_usize(v: &str) -> Result<usize, String> {
    usize::try_from(parse_size(v)?).map_err(|_| too_large())
}

pub(crate) fn parse_size_or_unlimited(v: &str) -> Result<Option<u64>, String> {
    if v == "unlimited" {
        Ok(None)
    } else {
        parse_size(v).map(Some)
    }
}

pub(crate) fn parse_duration(v: &str) -> Result<Duration, String> {
    let bad = || {
        format!("expected a positive duration (seconds, or with suffix ms, s, m or h), got {v:?}")
    };
    let r = match v.strip_suffix("ms") {
        Some(rest) => digits(rest).map(Duration::from_millis),
        None => scaled(v, &[("s", 1), ("m", 60), ("h", 3600), ("", 1)]).map(Duration::from_secs),
    };
    match r {
        Ok(d) if d.is_zero() => Err(bad()),
        Err(Bad::Syntax) => Err(bad()),
        Err(Bad::TooLarge) => Err(too_large()),
        Ok(d) => Ok(d),
    }
}

pub(crate) fn parse_bool(v: &str) -> Result<bool, String> {
    for t in ["1", "true", "yes", "on"] {
        if v.eq_ignore_ascii_case(t) {
            return Ok(true);
        }
    }
    for f in ["0", "false", "no", "off"] {
        if v.eq_ignore_ascii_case(f) {
            return Ok(false);
        }
    }
    Err(format!(
        "expected a boolean (1, true, yes, on, 0, false, no, off), got {v:?}"
    ))
}

pub(crate) fn parse_level(v: &str) -> Result<tracing::Level, String> {
    for (name, level) in [
        ("error", tracing::Level::ERROR),
        ("warn", tracing::Level::WARN),
        ("info", tracing::Level::INFO),
        ("debug", tracing::Level::DEBUG),
        ("trace", tracing::Level::TRACE),
    ] {
        if v.eq_ignore_ascii_case(name) {
            return Ok(level);
        }
    }
    Err(format!(
        "unknown level {v:?} (error, warn, info, debug, trace)"
    ))
}

fn parse_addr(v: &str) -> Result<SocketAddr, String> {
    v.parse::<SocketAddr>()
        .map_err(|_| format!("invalid address {v:?}"))
}

/// Largest unit that divides `n` exactly; re-parses with `parse_size`.
pub(crate) fn fmt_size(n: u64) -> String {
    for (unit, mult) in [("TiB", TIB), ("GiB", GIB), ("MiB", MIB), ("KiB", KIB)] {
        if n >= mult && n.is_multiple_of(mult) {
            return format!("{}{unit}", n / mult);
        }
    }
    n.to_string()
}

/// Whole seconds as `Ns`, else milliseconds; re-parses with `parse_duration`.
pub(crate) fn fmt_duration(d: Duration) -> String {
    if d.subsec_nanos() == 0 {
        format!("{}s", d.as_secs())
    } else {
        format!("{}ms", d.as_millis())
    }
}

fn fmt_usize_size(n: usize) -> String {
    fmt_size(u64::try_from(n).unwrap_or(u64::MAX))
}

// ---------------------------------------------------------------- the table

/// Every option, in application order within a layer: `--max-body-bytes` precedes the two
/// specific body caps so that the specific flag wins within one layer.
pub(crate) static OPTIONS: &[Opt] = &[
    Opt {
        flag: "--listen",
        value: "ADDR",
        kind: Kind::Value,
        help: "Address to bind, IP:PORT (IPv6 as [::1]:PORT).",
        fields: &[],
        apply: |s, v| {
            s.listen = parse_addr(v)?;
            Ok(())
        },
        show: |s| s.listen.to_string(),
    },
    Opt {
        flag: "--init",
        value: "",
        kind: Kind::Switch,
        help: "Create the repository if WORKDIR has none.",
        fields: &[],
        apply: |s, v| {
            s.init = parse_bool(v)?;
            Ok(())
        },
        show: |s| s.init.to_string(),
    },
    Opt {
        flag: "--log",
        value: "LEVEL",
        kind: Kind::Value,
        help: "Log level: error, warn, info, debug or trace.",
        fields: &[],
        apply: |s, v| {
            s.log = parse_level(v)?;
            Ok(())
        },
        show: |s| s.log.as_str().to_ascii_lowercase(),
    },
    Opt {
        flag: "--max-body-bytes",
        value: "SIZE",
        kind: Kind::Value,
        help: "Sets both --max-put-body-bytes and --max-import-body-bytes.",
        fields: &["max_put_body_bytes", "max_import_body_bytes"],
        apply: |s, v| {
            let n = parse_size(v)?;
            s.http.max_put_body_bytes = n;
            s.http.max_import_body_bytes = Some(n);
            Ok(())
        },
        show: |_| "unset".to_owned(),
    },
    Opt {
        flag: "--max-put-body-bytes",
        value: "SIZE",
        kind: Kind::Value,
        help: "Body cap of POST .../records, also the per-record cap.",
        fields: &["max_put_body_bytes"],
        apply: |s, v| {
            s.http.max_put_body_bytes = parse_size(v)?;
            Ok(())
        },
        show: |s| fmt_size(s.http.max_put_body_bytes),
    },
    Opt {
        flag: "--max-import-body-bytes",
        value: "SIZE",
        kind: Kind::Value,
        help: "Body cap of POST .../import, or unlimited (the import is streamed).",
        fields: &["max_import_body_bytes"],
        apply: |s, v| {
            s.http.max_import_body_bytes = parse_size_or_unlimited(v)?;
            Ok(())
        },
        show: |s| {
            s.http
                .max_import_body_bytes
                .map_or_else(|| "unlimited".to_owned(), fmt_size)
        },
    },
    Opt {
        flag: "--max-put-records",
        value: "N",
        kind: Kind::Value,
        help: "Records per POST .../records, at most 1000000.",
        fields: &["max_put_records"],
        apply: |s, v| {
            s.http.max_put_records = parse_count(v)?;
            Ok(())
        },
        show: |s| s.http.max_put_records.to_string(),
    },
    Opt {
        flag: "--max-json-body-bytes",
        value: "SIZE",
        kind: Kind::Value,
        help: "Cap of JSON request bodies.",
        fields: &["max_json_body_bytes"],
        apply: |s, v| {
            s.http.max_json_body_bytes = parse_size_usize(v)?;
            Ok(())
        },
        show: |s| fmt_usize_size(s.http.max_json_body_bytes),
    },
    Opt {
        flag: "--max-export-page-bytes",
        value: "SIZE",
        kind: Kind::Value,
        help: "Upper bound of max_bytes on export, at most 1GiB.",
        fields: &["max_export_page_bytes"],
        apply: |s, v| {
            s.http.max_export_page_bytes = parse_size(v)?;
            Ok(())
        },
        show: |s| fmt_size(s.http.max_export_page_bytes),
    },
    Opt {
        flag: "--request-timeout",
        value: "DURATION",
        kind: Kind::Value,
        help: "Timeout of routes without a streamed request body.",
        fields: &["request_timeout"],
        apply: |s, v| {
            s.http.request_timeout = parse_duration(v)?;
            Ok(())
        },
        show: |s| fmt_duration(s.http.request_timeout),
    },
    Opt {
        flag: "--body-idle-timeout",
        value: "DURATION",
        kind: Kind::Value,
        help: "Longest gap between two request-body chunks.",
        fields: &["body_idle_timeout"],
        apply: |s, v| {
            s.http.body_idle_timeout = parse_duration(v)?;
            Ok(())
        },
        show: |s| fmt_duration(s.http.body_idle_timeout),
    },
    Opt {
        flag: "--max-concurrent-requests",
        value: "N",
        kind: Kind::Value,
        help: "Requests in flight, response streaming included, at most 384.",
        fields: &["max_concurrent_requests"],
        apply: |s, v| {
            s.http.max_concurrent_requests = parse_count(v)?;
            Ok(())
        },
        show: |s| s.http.max_concurrent_requests.to_string(),
    },
    Opt {
        flag: "--max-running-jobs",
        value: "N",
        kind: Kind::Value,
        help: "Jobs running at once, at most 1024.",
        fields: &["max_running_jobs"],
        apply: |s, v| {
            s.http.max_running_jobs = parse_count(v)?;
            Ok(())
        },
        show: |s| s.http.max_running_jobs.to_string(),
    },
    Opt {
        flag: "--max-retained-jobs",
        value: "N",
        kind: Kind::Value,
        help: "Finished jobs kept for GET /v1/jobs, at most 100000.",
        fields: &["max_retained_jobs"],
        apply: |s, v| {
            s.http.max_retained_jobs = parse_count(v)?;
            Ok(())
        },
        show: |s| s.http.max_retained_jobs.to_string(),
    },
    Opt {
        flag: "--max-job-wait",
        value: "DURATION",
        kind: Kind::Value,
        help: "Upper bound of wait on GET /v1/jobs/{id}, at most 1h.",
        fields: &["max_job_wait"],
        apply: |s, v| {
            s.http.max_job_wait = parse_duration(v)?;
            Ok(())
        },
        show: |s| fmt_duration(s.http.max_job_wait),
    },
    Opt {
        flag: "--max-connections",
        value: "N",
        kind: Kind::Value,
        help: "Open connections, at most 1000000.",
        fields: &["max_connections"],
        apply: |s, v| {
            s.serve.max_connections = parse_count(v)?;
            Ok(())
        },
        show: |s| s.serve.max_connections.to_string(),
    },
    Opt {
        flag: "--header-read-timeout",
        value: "DURATION",
        kind: Kind::Value,
        help: "Time allowed to receive a request head.",
        fields: &["header_read_timeout"],
        apply: |s, v| {
            s.serve.header_read_timeout = parse_duration(v)?;
            Ok(())
        },
        show: |s| fmt_duration(s.serve.header_read_timeout),
    },
    Opt {
        flag: "--max-head-bytes",
        value: "SIZE",
        kind: Kind::Value,
        help: "Largest request head, above 416KiB and at most 16MiB.",
        fields: &["max_head_bytes"],
        apply: |s, v| {
            s.serve.max_head_bytes = parse_size_usize(v)?;
            Ok(())
        },
        show: |s| fmt_usize_size(s.serve.max_head_bytes),
    },
    Opt {
        flag: "--drain-timeout",
        value: "DURATION",
        kind: Kind::Value,
        help: "How long a graceful shutdown waits for in-flight requests.",
        fields: &["drain_timeout"],
        apply: |s, v| {
            s.serve.drain_timeout = parse_duration(v)?;
            Ok(())
        },
        show: |s| fmt_duration(s.serve.drain_timeout),
    },
];

/// `"--max-body-bytes"` -> `"MASSTUFFY_MAX_BODY_BYTES"`.
pub(crate) fn env_name(flag: &str) -> String {
    let mut out = String::with_capacity(ENV_PREFIX.len() + flag.len());
    out.push_str(ENV_PREFIX);
    out.extend(flag.trim_start_matches("--").chars().map(|c| {
        if c == '-' {
            '_'
        } else {
            c.to_ascii_uppercase()
        }
    }));
    out
}

// ---------------------------------------------------------------- parsing

/// Parses the command line and the environment snapshot. Never reads the process environment
/// itself: `main` passes `std::env::vars_os()`, tests pass a `Vec`.
///
/// # Errors
/// A message for a usage error (exit code 2): unknown or duplicate flag, missing value, bad
/// value (prefixed with the flag, or with `VAR=value`), missing WORKDIR, empty or non-UTF-8 env
/// value, a value rejected by `HttpOptions`/`ServeOptions::validate`.
pub(crate) fn parse(
    args: impl IntoIterator<Item = OsString>,
    env: impl IntoIterator<Item = (OsString, OsString)>,
) -> Result<Command, String> {
    // 1. CLI pass: collect, do not apply yet.
    let mut it = args.into_iter();
    let mut cli: Vec<(usize, String)> = Vec::new();
    let mut positional: Option<PathBuf> = None;
    let mut seen: Vec<String> = Vec::new();
    while let Some(arg) = it.next() {
        let text = match arg.to_str() {
            Some(t) if t.starts_with("--") => t,
            _ => {
                if positional.is_some() {
                    return Err("only one WORKDIR is accepted".into());
                }
                positional = Some(PathBuf::from(arg));
                continue;
            }
        };
        let (flag, inline) = match text.split_once('=') {
            Some((f, v)) => (f, Some(v.to_owned())),
            None => (text, None),
        };
        match flag {
            "--help" => return Ok(Command::Help),
            "--version" => return Ok(Command::Version),
            _ => {}
        }
        if seen.iter().any(|s| s == flag) {
            return Err(format!("duplicate flag {flag}"));
        }
        seen.push(flag.to_owned());
        let Some((idx, opt)) = OPTIONS.iter().enumerate().find(|(_, o)| o.flag == flag) else {
            return Err(format!("unknown flag {flag}"));
        };
        let value = match opt.kind {
            Kind::Switch => {
                if inline.is_some() {
                    return Err(format!("{flag} takes no value"));
                }
                "true".to_owned()
            }
            Kind::Value => match inline {
                Some(v) => v,
                None => it
                    .next()
                    .ok_or_else(|| format!("{flag} needs a value"))?
                    .into_string()
                    .map_err(|_| format!("{flag}: value is not UTF-8"))?,
            },
        };
        cli.push((idx, value));
    }

    // 2. Environment snapshot (keys are case-sensitive; non-UTF-8 keys are dropped).
    let mut vars: BTreeMap<String, OsString> = BTreeMap::new();
    for (k, v) in env {
        if let Ok(k) = k.into_string()
            && k.starts_with(ENV_PREFIX)
        {
            vars.insert(k, v);
        }
    }

    // 3. Defaults, 4. env layer (table order).
    let mut s = Settings::default();
    let mut sources = vec![Source::Default; OPTIONS.len()];
    for (opt, src) in OPTIONS.iter().zip(sources.iter_mut()) {
        let name = env_name(opt.flag);
        let Some(raw) = vars.get(&name) else { continue };
        let v = raw
            .to_str()
            .ok_or_else(|| format!("{name}: value is not UTF-8"))?;
        if v.is_empty() {
            return Err(format!(
                "{name} is set but empty (unset it to use the default)"
            ));
        }
        (opt.apply)(&mut s, v).map_err(|e| format!("{name}={v:?}: {e}"))?;
        *src = Source::Env;
    }

    // 5. CLI layer (table order, not command-line order).
    for (i, (opt, src)) in OPTIONS.iter().zip(sources.iter_mut()).enumerate() {
        if let Some((_, value)) = cli.iter().find(|(idx, _)| *idx == i) {
            (opt.apply)(&mut s, value).map_err(|e| format!("{}: {e}", opt.flag))?;
            *src = Source::Cli;
        }
    }

    // 6. Workdir. A malformed (empty) variable is an error even when the argument overrides
    // it, like every other option (D4).
    if vars.get(WORKDIR_ENV).is_some_and(|raw| raw.is_empty()) {
        return Err(format!(
            "{WORKDIR_ENV} is set but empty (unset it to use the default)"
        ));
    }
    let workdir = match positional {
        Some(p) => p,
        None => match vars.get(WORKDIR_ENV) {
            Some(raw) => PathBuf::from(raw),
            None => return Err("missing WORKDIR (argument or MASSTUFFY_WORKDIR)".into()),
        },
    };

    // 7. Validation, naming the option that supplied the bad value.
    if let Err(e) = s.http.validate().and_then(|()| s.serve.validate()) {
        let field = match &e {
            OptionsError::Zero(n) | OptionsError::Range(n) => Some(*n),
            _ => None,
        };
        let culprit = field.and_then(|n| {
            OPTIONS
                .iter()
                .zip(&sources)
                .enumerate()
                .filter(|(_, (o, src))| o.fields.contains(&n) && **src != Source::Default)
                .max_by_key(|(i, (_, src))| (src.rank(), *i))
                .map(|(_, (o, src))| (o, *src))
        });
        return Err(match culprit {
            Some((o, Source::Cli)) => format!("{}: {e}", o.flag),
            Some((o, _)) => format!("{}: {e}", env_name(o.flag)),
            None => e.to_string(),
        });
    }

    // 8. Unknown variables.
    let known: Vec<String> = OPTIONS.iter().map(|o| env_name(o.flag)).collect();
    let warnings = vars
        .keys()
        .filter(|k| {
            k.as_str() != WORKDIR_ENV
                && k.as_str() != RAW_IMPORT_CHECK_ENV
                && !known.iter().any(|n| n == *k)
        })
        .map(|k| format!("unknown environment variable {k} ignored (see masstuffy-server --help)"))
        .collect();

    Ok(Command::Run(Box::new(Args {
        workdir,
        settings: s,
        sources,
        warnings,
    })))
}

// ---------------------------------------------------------------- help and docs

/// Full `--help` text (ends with a newline).
pub(crate) fn help_text() -> String {
    let d = Settings::default();
    let mut out = String::from(
        "usage: masstuffy-server [WORKDIR] [OPTIONS]\n\
\n\
Serves the HTTP API of one Masstuffy repository. Every option can also be set by the\n\
environment variable shown; a command-line flag overrides the variable, which overrides\n\
the default. Flags accept --flag VALUE or --flag=VALUE.\n\
\n\
\x20 WORKDIR\n      Repository directory (required). [env: MASSTUFFY_WORKDIR]\n",
    );
    for o in OPTIONS {
        out.push_str("  ");
        out.push_str(o.flag);
        if !o.value.is_empty() {
            out.push(' ');
            out.push_str(o.value);
        }
        let suffix = if o.kind == Kind::Switch { "=1|0" } else { "" };
        out.push_str(&format!(
            "\n      {} [env: {}{suffix}] [default: {}]\n",
            o.help,
            env_name(o.flag),
            (o.show)(&d)
        ));
    }
    out.push_str(
        "  --help\n      Print this help.\n  --version\n      Print the version.\n\
\n\
SIZE: bytes, or a number with suffix KiB, MiB, GiB or TiB (e.g. 64MiB).\n\
DURATION: seconds, or a number with suffix ms, s, m or h (e.g. 500ms, 30s, 2m).\n\
Booleans in the environment: 1, true, yes, on, 0, false, no, off.\n",
    );
    out.push_str(&format!(
        "Also read by the engine: {RAW_IMPORT_CHECK_ENV}=full|header (default full).\n"
    ));
    out.push_str(
        "No authentication or TLS: bind to loopback or put an authenticating reverse proxy in front.\n",
    );
    out
}

/// Markdown table for `docs/http-api.md` section 8 (ends with a newline). Test-only: the docs are
/// checked against it.
#[cfg(test)]
pub(crate) fn markdown_table() -> String {
    let d = Settings::default();
    let mut out = String::from(
        "| Flag | Environment variable | Default | Meaning |\n|---|---|---|---|\n\
| `WORKDIR` (argument) | `MASSTUFFY_WORKDIR` | required | Repository directory. |\n",
    );
    for o in OPTIONS {
        let flag = if o.value.is_empty() {
            o.flag.to_owned()
        } else {
            format!("{} {}", o.flag, o.value)
        };
        out.push_str(&format!(
            "| `{flag}` | `{}` | `{}` | {} |\n",
            env_name(o.flag),
            (o.show)(&d),
            o.help
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect()
    }

    fn p(args: &[&str], e: &[(&str, &str)]) -> Result<Command, String> {
        parse(args.iter().map(OsString::from), env(e))
    }

    fn run(args: &[&str], e: &[(&str, &str)]) -> Args {
        match p(args, e) {
            Ok(Command::Run(a)) => *a,
            Ok(_) => panic!("not a run command"),
            Err(e) => panic!("parse failed: {e}"),
        }
    }

    fn err(args: &[&str], e: &[(&str, &str)]) -> String {
        match p(args, e) {
            Err(m) => m,
            Ok(_) => panic!("expected an error for {args:?} {e:?}"),
        }
    }

    fn idx(flag: &str) -> usize {
        OPTIONS.iter().position(|o| o.flag == flag).unwrap()
    }

    #[test]
    fn size_parser() {
        assert_eq!(parse_size("1"), Ok(1));
        assert_eq!(parse_size("64MiB"), Ok(1 << 26));
        assert_eq!(parse_size("1KiB"), Ok(1024));
        assert_eq!(parse_size("2TiB"), Ok(2 << 40));
        assert_eq!(parse_size("18446744073709551615"), Ok(u64::MAX));
        for bad in [
            "", "0", "0MiB", "+5", "-1", " 5", "5 ", "5 MiB", "5mib", "5MB", "5K", "MiB", "1.5MiB",
            "5é", "٣",
        ] {
            assert!(parse_size(bad).is_err(), "{bad:?}");
        }
        for big in ["18446744073709551616", "16777216TiB"] {
            assert_eq!(parse_size(big), Err("value too large".to_owned()), "{big}");
        }
    }

    #[test]
    fn size_or_unlimited() {
        assert_eq!(parse_size_or_unlimited("unlimited"), Ok(None));
        assert_eq!(parse_size_or_unlimited("1"), Ok(Some(1)));
        assert!(parse_size_or_unlimited("Unlimited").is_err());
    }

    #[test]
    fn duration_parser() {
        let s = Duration::from_secs;
        assert_eq!(parse_duration("30"), Ok(s(30)));
        assert_eq!(parse_duration("30s"), Ok(s(30)));
        assert_eq!(parse_duration("500ms"), Ok(Duration::from_millis(500)));
        assert_eq!(parse_duration("2m"), Ok(s(120)));
        assert_eq!(parse_duration("1h"), Ok(s(3600)));
        for bad in ["", "0", "0ms", "1.5s", "1d", "1 s", "-1", "s", "ms"] {
            assert!(parse_duration(bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            parse_duration("18446744073709551615h"),
            Err("value too large".to_owned())
        );
    }

    #[test]
    fn count_parser() {
        assert_eq!(parse_count("1"), Ok(1));
        assert_eq!(parse_count("384"), Ok(384));
        for bad in ["0", "+1", "1k", "", "1e3", "99999999999999999999999"] {
            assert!(parse_count(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn bool_parser() {
        for t in ["1", "true", "yes", "on", "TRUE", "Yes"] {
            assert_eq!(parse_bool(t), Ok(true), "{t}");
        }
        for f in ["0", "false", "no", "off", "OFF"] {
            assert_eq!(parse_bool(f), Ok(false), "{f}");
        }
        for bad in ["", "2", "y", "enable"] {
            assert!(parse_bool(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn level_parser() {
        for l in ["error", "warn", "info", "debug", "trace", "DEBUG"] {
            assert!(parse_level(l).is_ok(), "{l}");
        }
        assert!(parse_level("loud").is_err());
        assert!(parse_level("").is_err());
    }

    #[test]
    fn fmt_round_trip() {
        for n in [
            1u64,
            1023,
            1024,
            524_288,
            64 << 20,
            1 << 30,
            3 << 40,
            u64::MAX,
        ] {
            assert_eq!(parse_size(&fmt_size(n)), Ok(n), "{n}");
        }
        assert_eq!(fmt_size(64 << 20), "64MiB");
        assert_eq!(fmt_size(524_288), "512KiB");
        assert_eq!(fmt_size(1000), "1000");
        for ms in [1u64, 999, 1000, 30_000, 3_600_000] {
            let d = Duration::from_millis(ms);
            assert_eq!(parse_duration(&fmt_duration(d)), Ok(d), "{ms}");
        }
        assert_eq!(fmt_duration(Duration::from_secs(30)), "30s");
        assert_eq!(fmt_duration(Duration::from_millis(500)), "500ms");
    }

    #[test]
    fn options_table_invariants() {
        let mut flags = std::collections::BTreeSet::new();
        let mut names = std::collections::BTreeSet::new();
        for o in OPTIONS {
            assert!(o.flag.starts_with("--"), "{}", o.flag);
            assert!(
                o.flag
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
                "{}",
                o.flag
            );
            assert!(flags.insert(o.flag), "duplicate {}", o.flag);
            let n = env_name(o.flag);
            assert_ne!(n, WORKDIR_ENV);
            assert_ne!(n, RAW_IMPORT_CHECK_ENV);
            assert!(names.insert(n));
            assert_eq!(o.value.is_empty(), o.kind == Kind::Switch, "{}", o.flag);
            for t in [o.value, o.help] {
                assert!(!t.contains('|') && !t.contains('\n'), "{}", o.flag);
            }
            assert!(o.help.ends_with('.'), "{}", o.flag);
        }
        assert_eq!(env_name("--max-body-bytes"), "MASSTUFFY_MAX_BODY_BYTES");
        assert!(idx("--max-body-bytes") < idx("--max-put-body-bytes"));
        assert!(idx("--max-body-bytes") < idx("--max-import-body-bytes"));
        // A new HttpOptions or ServeOptions field needs a new table row and an entry here.
        for f in [
            "max_put_body_bytes",
            "max_put_records",
            "max_import_body_bytes",
            "max_json_body_bytes",
            "max_export_page_bytes",
            "request_timeout",
            "body_idle_timeout",
            "max_concurrent_requests",
            "max_running_jobs",
            "max_retained_jobs",
            "max_job_wait",
            "max_connections",
            "max_head_bytes",
            "header_read_timeout",
            "drain_timeout",
        ] {
            assert!(OPTIONS.iter().any(|o| o.fields.contains(&f)), "{f}");
        }
        let mut d = Settings::default();
        for o in OPTIONS {
            if o.kind == Kind::Value && o.flag != "--max-body-bytes" {
                let shown = (o.show)(&d);
                (o.apply)(&mut d, &shown).unwrap_or_else(|e| panic!("{}: {e}", o.flag));
                assert_eq!((o.show)(&d), shown, "{}", o.flag);
            }
        }
    }

    #[test]
    fn parse_defaults() {
        let a = run(&["/w"], &[]);
        assert_eq!(a.workdir, PathBuf::from("/w"));
        assert_eq!(a.settings.listen, "127.0.0.1:8470".parse().unwrap());
        assert!(!a.settings.init);
        assert_eq!(a.settings.log, tracing::Level::INFO);
        assert_eq!(a.settings.http.max_put_body_bytes, 64 << 20);
        assert_eq!(a.settings.http.max_import_body_bytes, None);
        assert_eq!(a.settings.serve.max_connections, 256);
        assert!(a.sources.iter().all(|s| *s == Source::Default));
        assert!(a.warnings.is_empty());
    }

    /// Non-default value for each flag (`--max-body-bytes` is tested separately).
    const ALL: &[(&str, &str)] = &[
        ("--listen", "0.0.0.0:9"),
        ("--log", "debug"),
        ("--max-put-body-bytes", "1MiB"),
        ("--max-import-body-bytes", "2MiB"),
        ("--max-put-records", "11"),
        ("--max-json-body-bytes", "3MiB"),
        ("--max-export-page-bytes", "4MiB"),
        ("--request-timeout", "7s"),
        ("--body-idle-timeout", "8s"),
        ("--max-concurrent-requests", "6"),
        ("--max-running-jobs", "5"),
        ("--max-retained-jobs", "4"),
        ("--max-job-wait", "2m"),
        ("--max-connections", "3"),
        ("--header-read-timeout", "9s"),
        ("--max-head-bytes", "1MiB"),
        ("--drain-timeout", "500ms"),
    ];

    fn check_all(a: &Args) {
        let (s, h, v) = (&a.settings, &a.settings.http, &a.settings.serve);
        assert_eq!(s.listen, "0.0.0.0:9".parse().unwrap());
        assert!(s.init);
        assert_eq!(s.log, tracing::Level::DEBUG);
        assert_eq!(h.max_put_body_bytes, 1 << 20);
        assert_eq!(h.max_import_body_bytes, Some(2 << 20));
        assert_eq!(h.max_put_records, 11);
        assert_eq!(h.max_json_body_bytes, 3 << 20);
        assert_eq!(h.max_export_page_bytes, 4 << 20);
        assert_eq!(h.request_timeout, Duration::from_secs(7));
        assert_eq!(h.body_idle_timeout, Duration::from_secs(8));
        assert_eq!(h.max_concurrent_requests, 6);
        assert_eq!(h.max_running_jobs, 5);
        assert_eq!(h.max_retained_jobs, 4);
        assert_eq!(h.max_job_wait, Duration::from_secs(120));
        assert_eq!(v.max_connections, 3);
        assert_eq!(v.header_read_timeout, Duration::from_secs(9));
        assert_eq!(v.max_head_bytes, 1 << 20);
        assert_eq!(v.drain_timeout, Duration::from_millis(500));
    }

    #[test]
    fn parse_all_flags() {
        let mut spaced = vec!["/w", "--init"];
        let mut joined: Vec<String> = vec!["--init".into(), "/w".into()];
        for (f, v) in ALL {
            spaced.push(f);
            spaced.push(v);
            joined.push(format!("{f}={v}"));
        }
        let a = run(&spaced, &[]);
        check_all(&a);
        let j: Vec<&str> = joined.iter().map(String::as_str).collect();
        check_all(&run(&j, &[]));
        for (o, s) in OPTIONS.iter().zip(&a.sources) {
            if o.flag != "--max-body-bytes" {
                assert_eq!(*s, Source::Cli, "{}", o.flag);
            }
        }

        let a = run(&["/w", "--max-body-bytes", "100"], &[]);
        assert_eq!(a.settings.http.max_put_body_bytes, 100);
        assert_eq!(a.settings.http.max_import_body_bytes, Some(100));
        let a = run(
            &[
                "/w",
                "--max-import-body-bytes",
                "unlimited",
                "--max-body-bytes",
                "100",
            ],
            &[],
        );
        assert_eq!(a.settings.http.max_import_body_bytes, None);
        assert_eq!(a.settings.http.max_put_body_bytes, 100);
    }

    #[test]
    fn parse_units_on_cli() {
        let a = run(
            &[
                "/w",
                "--max-body-bytes",
                "2MiB",
                "--request-timeout",
                "500ms",
                "--drain-timeout",
                "1m",
            ],
            &[],
        );
        assert_eq!(a.settings.http.max_put_body_bytes, 2 << 20);
        assert_eq!(a.settings.http.request_timeout, Duration::from_millis(500));
        assert_eq!(a.settings.serve.drain_timeout, Duration::from_secs(60));
    }

    #[test]
    fn parse_errors() {
        for bad in [
            &[][..],
            &["/a", "/b"],
            &["/w", "--nope"],
            &["/w", "--init", "--init"],
            &["/w", "--listen"],
            &["/w", "--max-body-bytes", "0"],
            &["/w", "--max-body-bytes", "+5"],
            &["/w", "--listen", "nope"],
            &["/w", "--log", "loud"],
            &["/w", "--log"],
            &["/w", "--init=1"],
            &["/w", "--max-import-body-bytes", "0"],
            &["/w", "--max-head-bytes", "64KiB"],
        ] {
            assert!(p(bad, &[]).is_err(), "{bad:?}");
        }
        assert!(
            err(&["/w", "--max-put-records", "1000001"], &[]).starts_with("--max-put-records:")
        );
        let m = err(&["/w", "--max-concurrent-requests", "385"], &[]);
        assert!(m.starts_with("--max-concurrent-requests:"), "{m}");
    }

    #[test]
    fn parse_help_version() {
        assert!(matches!(p(&["--help"], &[]), Ok(Command::Help)));
        assert!(matches!(p(&["/w", "--version"], &[]), Ok(Command::Version)));
        assert!(matches!(
            p(&["--help"], &[("MASSTUFFY_MAX_CONNECTIONS", "abc")]),
            Ok(Command::Help)
        ));
    }

    #[test]
    fn env_sets_every_option() {
        let mut pairs: Vec<(String, String)> = vec![
            (WORKDIR_ENV.into(), "/e".into()),
            (env_name("--init"), "1".into()),
        ];
        for (f, v) in ALL {
            pairs.push((env_name(f), (*v).into()));
        }
        let refs: Vec<(&str, &str)> = pairs
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let a = run(&[], &refs);
        check_all(&a);
        assert_eq!(a.workdir, PathBuf::from("/e"));
        for (o, s) in OPTIONS.iter().zip(&a.sources) {
            if o.flag != "--max-body-bytes" {
                assert_eq!(*s, Source::Env, "{}", o.flag);
            }
        }
        assert!(a.warnings.is_empty());
    }

    #[test]
    fn cli_overrides_env() {
        let a = run(
            &["/w", "--listen", "127.0.0.1:2"],
            &[
                ("MASSTUFFY_LISTEN", "0.0.0.0:1"),
                ("MASSTUFFY_LOG", "trace"),
                ("MASSTUFFY_WORKDIR", "/e"),
            ],
        );
        assert_eq!(a.settings.listen, "127.0.0.1:2".parse().unwrap());
        assert_eq!(a.sources[idx("--listen")], Source::Cli);
        assert_eq!(a.settings.log, tracing::Level::TRACE);
        assert_eq!(a.sources[idx("--log")], Source::Env);
        assert_eq!(a.workdir, PathBuf::from("/w"));
    }

    #[test]
    fn layer_order_body_caps() {
        let a = run(
            &["/w", "--max-body-bytes", "100"],
            &[("MASSTUFFY_MAX_PUT_BODY_BYTES", "10")],
        );
        assert_eq!(a.settings.http.max_put_body_bytes, 100);
        assert_eq!(a.settings.http.max_import_body_bytes, Some(100));
        let a = run(
            &["/w", "--max-put-body-bytes", "10"],
            &[("MASSTUFFY_MAX_BODY_BYTES", "100")],
        );
        assert_eq!(a.settings.http.max_put_body_bytes, 10);
        assert_eq!(a.settings.http.max_import_body_bytes, Some(100));
        let a = run(
            &["/w"],
            &[
                ("MASSTUFFY_MAX_BODY_BYTES", "100"),
                ("MASSTUFFY_MAX_IMPORT_BODY_BYTES", "unlimited"),
            ],
        );
        assert_eq!(a.settings.http.max_put_body_bytes, 100);
        assert_eq!(a.settings.http.max_import_body_bytes, None);
    }

    #[test]
    fn env_init_bool() {
        for t in ["1", "true", "YES", "on"] {
            assert!(run(&["/w"], &[("MASSTUFFY_INIT", t)]).settings.init, "{t}");
        }
        for f in ["0", "false", "no", "off"] {
            assert!(!run(&["/w"], &[("MASSTUFFY_INIT", f)]).settings.init, "{f}");
        }
        assert!(err(&["/w"], &[("MASSTUFFY_INIT", "maybe")]).contains("MASSTUFFY_INIT=\"maybe\""));
        assert!(
            run(&["/w", "--init"], &[("MASSTUFFY_INIT", "0")])
                .settings
                .init
        );
    }

    #[test]
    fn env_errors() {
        for (args, e, needle) in [
            (
                &["/w"][..],
                ("MASSTUFFY_MAX_CONNECTIONS", "abc"),
                "MASSTUFFY_MAX_CONNECTIONS",
            ),
            (&["/w"], ("MASSTUFFY_LISTEN", ""), "set but empty"),
            (
                &[],
                ("MASSTUFFY_WORKDIR", ""),
                "MASSTUFFY_WORKDIR is set but empty",
            ),
            // Checked even when the argument overrides it (D4).
            (
                &["/w"],
                ("MASSTUFFY_WORKDIR", ""),
                "MASSTUFFY_WORKDIR is set but empty",
            ),
            (
                &["/w"],
                ("MASSTUFFY_REQUEST_TIMEOUT", "0"),
                "MASSTUFFY_REQUEST_TIMEOUT",
            ),
            (&["/w"], ("MASSTUFFY_LISTEN", "nope"), "MASSTUFFY_LISTEN"),
        ] {
            let m = err(args, &[e]);
            assert!(m.contains(needle), "{m}");
            assert!(m.contains(e.0), "{m}");
        }
        let m = err(&["/w"], &[("MASSTUFFY_MAX_CONCURRENT_REQUESTS", "500")]);
        assert!(m.starts_with("MASSTUFFY_MAX_CONCURRENT_REQUESTS:"), "{m}");
        assert!(m.contains("out of range"), "{m}");
    }

    #[cfg(unix)]
    #[test]
    fn env_non_utf8() {
        use std::os::unix::ffi::OsStringExt;
        let bad = vec![(
            OsString::from("MASSTUFFY_LOG"),
            OsString::from_vec(vec![0xff]),
        )];
        match parse([OsString::from("/w")], bad) {
            Err(m) => assert!(m.contains("not UTF-8"), "{m}"),
            Ok(_) => panic!("expected an error"),
        }
        let wd = vec![(
            OsString::from(WORKDIR_ENV),
            OsString::from_vec(b"/w\xff".to_vec()),
        )];
        match parse([], wd) {
            Ok(Command::Run(a)) => {
                assert_eq!(
                    a.workdir,
                    PathBuf::from(OsString::from_vec(b"/w\xff".to_vec()))
                );
            }
            _ => panic!("expected a run command"),
        }
        let key = vec![(
            OsString::from_vec(b"MASSTUFFY_\xff".to_vec()),
            OsString::from("x"),
        )];
        match parse([OsString::from("/w")], key) {
            Ok(Command::Run(a)) => assert!(a.warnings.is_empty()),
            _ => panic!("expected a run command"),
        }
    }

    #[test]
    fn env_invalid_even_if_overridden() {
        let m = err(
            &["/w", "--listen", "1.2.3.4:5"],
            &[("MASSTUFFY_LISTEN", "nope")],
        );
        assert!(m.contains("MASSTUFFY_LISTEN"), "{m}");
    }

    #[test]
    fn validation_source_rank() {
        let m = err(&["/w"], &[("MASSTUFFY_MAX_EXPORT_PAGE_BYTES", "2GiB")]);
        assert!(m.starts_with("MASSTUFFY_MAX_EXPORT_PAGE_BYTES:"), "{m}");
        let m = err(
            &["/w", "--max-export-page-bytes", "2GiB"],
            &[("MASSTUFFY_MAX_EXPORT_PAGE_BYTES", "1MiB")],
        );
        assert!(m.starts_with("--max-export-page-bytes:"), "{m}");
    }

    #[test]
    fn unknown_vars_warn() {
        let a = run(
            &["/w"],
            &[
                ("MASSTUFFY_LISTN", "x"),
                ("MASSTUFFY_SERVER_BIN", "y"),
                ("MASSTUFFY_RAW_IMPORT_CHECK", "header"),
                ("OTHER", "z"),
                ("masstuffy_listen", "q"),
            ],
        );
        assert_eq!(
            a.warnings,
            [
                "unknown environment variable MASSTUFFY_LISTN ignored (see masstuffy-server --help)",
                "unknown environment variable MASSTUFFY_SERVER_BIN ignored (see masstuffy-server --help)"
            ]
        );
    }

    #[test]
    fn help_lists_every_option() {
        let h = help_text();
        let d = Settings::default();
        for o in OPTIONS {
            assert!(h.contains(o.flag));
            assert!(h.contains(&env_name(o.flag)));
            assert!(
                h.contains(&format!("[default: {}]", (o.show)(&d))),
                "{}",
                o.flag
            );
        }
        for t in [
            "MASSTUFFY_WORKDIR",
            "MASSTUFFY_RAW_IMPORT_CHECK",
            "[default: 127.0.0.1:8470]",
            "[default: 64MiB]",
            "[default: 30s]",
            "[default: unlimited]",
        ] {
            assert!(h.contains(t), "{t}");
        }
        assert!(h.ends_with('\n'));
    }

    #[test]
    fn markdown_table_shape() {
        let t = markdown_table();
        assert_eq!(t.lines().count(), OPTIONS.len() + 3);
        for l in t.lines() {
            assert!(l.starts_with("| ") || l.starts_with("|---"), "{l}");
            assert_eq!(l.matches('|').count(), 5, "{l}");
        }
    }

    #[test]
    fn docs_http_api_has_the_table() {
        const DOC: &str = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/http-api.md"
        ));
        assert!(
            DOC.contains(&markdown_table()),
            "docs/http-api.md section 8 is out of date: paste the output of \
             `cargo test -p masstuffy-http --features server --bin masstuffy-server -- --ignored --nocapture print_markdown_table` into it"
        );
    }

    #[test]
    #[ignore = "prints the table to paste into docs/http-api.md"]
    fn print_markdown_table() {
        print!("{}", markdown_table());
    }
}
