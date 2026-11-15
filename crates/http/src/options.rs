//! Limits of the HTTP layer and of the accept loop.

use std::time::Duration;

use crate::locator::MAX_LOCATOR_TOKEN_LEN;

const KIB: u64 = 1 << 10;
const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;
/// Largest `max_concurrent_requests`.
pub const MAX_CONCURRENT_REQUESTS: u64 = 384;
const DAY: Duration = Duration::from_secs(86_400);

/// Limits of the HTTP layer. All sizes in bytes. `Default` gives the documented values.
#[derive(Debug, Clone)]
pub struct HttpOptions {
    /// `POST .../records` body cap (also caps each record). Default 64 MiB.
    pub max_put_body_bytes: u64,
    /// Records per `POST .../records`. Default 10 000.
    pub max_put_records: usize,
    /// `POST .../import` body cap; `None` = unbounded (the import is streamed). Default `None`.
    pub max_import_body_bytes: Option<u64>,
    /// JSON request bodies. Default 32 MiB.
    pub max_json_body_bytes: usize,
    /// Upper bound of `?max_bytes` on export. Default 64 MiB.
    pub max_export_page_bytes: u64,
    /// Timeout of routes without a streamed request body. Default 60 s.
    pub request_timeout: Duration,
    /// Longest gap between two request-body chunks. Default 30 s.
    pub body_idle_timeout: Duration,
    /// Requests in flight (response body streaming included). Default 256.
    pub max_concurrent_requests: usize,
    /// Jobs running at once. Default 8.
    pub max_running_jobs: usize,
    /// Finished jobs kept for `GET /v1/jobs`. Default 256.
    pub max_retained_jobs: usize,
    /// Upper bound of `?wait=` on `GET /v1/jobs/{id}`. Default 60 s.
    pub max_job_wait: Duration,
}

impl Default for HttpOptions {
    fn default() -> Self {
        HttpOptions {
            max_put_body_bytes: 64 * MIB,
            max_put_records: 10_000,
            max_import_body_bytes: None,
            max_json_body_bytes: 32 * 1024 * 1024,
            max_export_page_bytes: 64 * MIB,
            request_timeout: Duration::from_secs(60),
            body_idle_timeout: Duration::from_secs(30),
            max_concurrent_requests: 256,
            max_running_jobs: 8,
            max_retained_jobs: 256,
            max_job_wait: Duration::from_secs(60),
        }
    }
}

/// Limits of the accept loop.
#[derive(Debug, Clone)]
pub struct ServeOptions {
    /// Open connections. Default 256.
    pub max_connections: usize,
    /// Time allowed to receive a request head. Default 30 s.
    pub header_read_timeout: Duration,
    /// How long a graceful shutdown waits for in-flight requests. Default 30 s.
    pub drain_timeout: Duration,
    /// Largest request head (hyper `max_buf_size`). Default 512 KiB.
    pub max_head_bytes: usize,
}

impl Default for ServeOptions {
    fn default() -> Self {
        ServeOptions {
            max_connections: 256,
            header_read_timeout: Duration::from_secs(30),
            drain_timeout: Duration::from_secs(30),
            max_head_bytes: 512 * 1024,
        }
    }
}

/// An invalid option.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OptionsError {
    /// A size, count or duration is zero.
    #[error("option {0} must be non-zero")]
    Zero(&'static str),
    /// A value is above its maximum (or below its minimum).
    #[error("option {0} is out of range")]
    Range(&'static str),
}

fn nonzero(v: u64, name: &'static str) -> Result<(), OptionsError> {
    if v == 0 {
        Err(OptionsError::Zero(name))
    } else {
        Ok(())
    }
}

fn dur(d: Duration, name: &'static str, min: Duration, max: Duration) -> Result<(), OptionsError> {
    if d.is_zero() {
        return Err(OptionsError::Zero(name));
    }
    if d < min || d > max {
        return Err(OptionsError::Range(name));
    }
    Ok(())
}

fn upto(v: u64, max: u64, name: &'static str) -> Result<(), OptionsError> {
    nonzero(v, name)?;
    if v > max {
        return Err(OptionsError::Range(name));
    }
    Ok(())
}

fn as_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

impl HttpOptions {
    /// Checks every field.
    ///
    /// # Errors
    ///
    /// `Zero` for a zero size, count or duration; `Range` for a value above its maximum
    /// (`max_put_records` 1 000 000, `max_export_page_bytes` 1 GiB, `max_concurrent_requests`
    /// 384, `max_running_jobs` 1024, `max_retained_jobs` 100 000, `max_job_wait` 3600 s,
    /// `request_timeout` and `body_idle_timeout` one day).
    pub fn validate(&self) -> Result<(), OptionsError> {
        nonzero(self.max_put_body_bytes, "max_put_body_bytes")?;
        upto(as_u64(self.max_put_records), 1_000_000, "max_put_records")?;
        if let Some(n) = self.max_import_body_bytes {
            nonzero(n, "max_import_body_bytes")?;
        }
        nonzero(as_u64(self.max_json_body_bytes), "max_json_body_bytes")?;
        upto(self.max_export_page_bytes, GIB, "max_export_page_bytes")?;
        let one_ms = Duration::from_millis(1);
        dur(self.request_timeout, "request_timeout", one_ms, DAY)?;
        dur(self.body_idle_timeout, "body_idle_timeout", one_ms, DAY)?;
        // Each in-flight upload parks one thread of tokio's blocking pool (512 by default) for its
        // WARC parse or the engine's import producer: the cap keeps room for the engine's own
        // blocking work (reads, prepares, fsyncs).
        upto(
            as_u64(self.max_concurrent_requests),
            MAX_CONCURRENT_REQUESTS,
            "max_concurrent_requests",
        )?;
        upto(as_u64(self.max_running_jobs), 1024, "max_running_jobs")?;
        upto(as_u64(self.max_retained_jobs), 100_000, "max_retained_jobs")?;
        dur(
            self.max_job_wait,
            "max_job_wait",
            one_ms,
            Duration::from_secs(3600),
        )?;
        Ok(())
    }
}

impl ServeOptions {
    /// Checks every field.
    ///
    /// # Errors
    ///
    /// `Zero` / `Range`: `max_connections` in `1..=1_000_000`; `max_head_bytes` above
    /// `MAX_LOCATOR_TOKEN_LEN + 64 KiB` and at most 16 MiB; each timeout in `1 ms..=86400 s`.
    pub fn validate(&self) -> Result<(), OptionsError> {
        upto(as_u64(self.max_connections), 1_000_000, "max_connections")?;
        nonzero(as_u64(self.max_head_bytes), "max_head_bytes")?;
        let min_head = as_u64(MAX_LOCATOR_TOKEN_LEN).saturating_add(64 * KIB);
        if as_u64(self.max_head_bytes) <= min_head || as_u64(self.max_head_bytes) > 16 * MIB {
            return Err(OptionsError::Range("max_head_bytes"));
        }
        let one_ms = Duration::from_millis(1);
        dur(self.header_read_timeout, "header_read_timeout", one_ms, DAY)?;
        dur(self.drain_timeout, "drain_timeout", one_ms, DAY)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zero_of(o: HttpOptions) -> Option<&'static str> {
        match o.validate() {
            Err(OptionsError::Zero(n)) => Some(n),
            _ => None,
        }
    }

    fn is_range<T>(r: Result<T, OptionsError>) -> bool {
        matches!(r, Err(OptionsError::Range(_)))
    }

    #[test]
    fn options_validate() {
        HttpOptions::default().validate().unwrap();
        ServeOptions::default().validate().unwrap();
        let d = HttpOptions::default;
        assert_eq!(
            zero_of(HttpOptions {
                max_put_body_bytes: 0,
                ..d()
            }),
            Some("max_put_body_bytes")
        );
        assert_eq!(
            zero_of(HttpOptions {
                max_put_records: 0,
                ..d()
            }),
            Some("max_put_records")
        );
        assert_eq!(
            zero_of(HttpOptions {
                max_import_body_bytes: Some(0),
                ..d()
            }),
            Some("max_import_body_bytes")
        );
        assert_eq!(
            zero_of(HttpOptions {
                max_json_body_bytes: 0,
                ..d()
            }),
            Some("max_json_body_bytes")
        );
        assert_eq!(
            zero_of(HttpOptions {
                max_export_page_bytes: 0,
                ..d()
            }),
            Some("max_export_page_bytes")
        );
        assert_eq!(
            zero_of(HttpOptions {
                request_timeout: Duration::ZERO,
                ..d()
            }),
            Some("request_timeout")
        );
        assert_eq!(
            zero_of(HttpOptions {
                body_idle_timeout: Duration::ZERO,
                ..d()
            }),
            Some("body_idle_timeout")
        );
        assert_eq!(
            zero_of(HttpOptions {
                max_concurrent_requests: 0,
                ..d()
            }),
            Some("max_concurrent_requests")
        );
        assert_eq!(
            zero_of(HttpOptions {
                max_running_jobs: 0,
                ..d()
            }),
            Some("max_running_jobs")
        );
        assert_eq!(
            zero_of(HttpOptions {
                max_retained_jobs: 0,
                ..d()
            }),
            Some("max_retained_jobs")
        );
        assert_eq!(
            zero_of(HttpOptions {
                max_job_wait: Duration::ZERO,
                ..d()
            }),
            Some("max_job_wait")
        );
        assert!(is_range(
            HttpOptions {
                max_put_records: 1_000_001,
                ..d()
            }
            .validate()
        ));
        assert!(is_range(
            HttpOptions {
                max_export_page_bytes: GIB + 1,
                ..d()
            }
            .validate()
        ));
        assert!(is_range(
            HttpOptions {
                max_job_wait: Duration::from_secs(3601),
                ..d()
            }
            .validate()
        ));

        let s = ServeOptions::default;
        assert!(is_range(
            ServeOptions {
                max_head_bytes: 64 * 1024,
                ..s()
            }
            .validate()
        ));
        assert!(is_range(
            ServeOptions {
                max_head_bytes: usize::MAX,
                ..s()
            }
            .validate()
        ));
        assert!(matches!(
            ServeOptions {
                max_connections: 0,
                ..s()
            }
            .validate(),
            Err(OptionsError::Zero(_))
        ));
        assert!(is_range(
            ServeOptions {
                drain_timeout: Duration::from_secs(86_401),
                ..s()
            }
            .validate()
        ));
    }
}
