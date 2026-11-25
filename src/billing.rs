//! What a check is estimated to cost, and the usage log an adapter can bill from.
//!
//! The price is the one the API publishes for the account (credits per 10 seconds of remote
//! command time). A check is charged for its command time, rounded up per check. A result
//! served from the cache, a check that was turned away and a check run on this machine cost
//! nothing. The figure here is an estimate made on the client: the account's ledger is the
//! authority.

use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// The published price for one account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Price {
    /// Credits per 10 seconds of remote command time. `None` when checks are free for the account.
    pub credits_per_10s: Option<u64>,
    /// The account is not charged for checks.
    pub no_charge: bool,
}

impl Price {
    pub fn from_status(status: &mcp_client::remote_checks::RemoteChecks) -> Self {
        Self {
            credits_per_10s: status.pricing.as_ref().map(|price| price.credits_per_10s),
            no_charge: status
                .credits
                .as_ref()
                .is_some_and(|credits| credits.no_charge),
        }
    }
}

/// The billing block of a result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Billing {
    /// Estimated Context Credits for this call. `null` when the price could not be read.
    pub credits_estimated: Option<u64>,
    /// Seconds of remote command time the estimate is for.
    pub billable_seconds: f64,
    /// Why the call is free, when it is: `cached`, `not_run_remotely`, `ran_locally`,
    /// `account_not_charged`.
    pub free_reason: Option<String>,
    /// Always true: the account's ledger is the authority.
    pub estimate: bool,
    /// The price the estimate used, when one was read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credits_per_10s: Option<u64>,
}

impl Billing {
    pub fn free(reason: &str) -> Self {
        Self {
            credits_estimated: Some(0),
            billable_seconds: 0.0,
            free_reason: Some(reason.to_string()),
            estimate: true,
            credits_per_10s: None,
        }
    }
}

/// How a check came out, as far as the price is concerned.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Usage {
    /// It ran remotely for this many seconds of command time.
    Remote { seconds: f64 },
    /// The result came from the cache.
    Cached,
    /// It was turned away and nothing ran remotely.
    NotRun,
    /// It ran on this machine.
    Local,
}

/// Credits for `seconds` of command time at `credits_per_10s`, rounded up per check.
pub fn credits_for(seconds: f64, credits_per_10s: u64) -> u64 {
    if !seconds.is_finite() || seconds <= 0.0 || credits_per_10s == 0 {
        return 0;
    }
    // In tenths of a credit-second to keep the rounding exact for clean inputs.
    let exact = seconds * credits_per_10s as f64 / 10.0;
    let rounded = exact.ceil();
    // 4.0000000001 from float noise is 4, not 5.
    if (exact - exact.round()).abs() < 1e-9 {
        exact.round() as u64
    } else {
        rounded as u64
    }
}

/// The billing block for one check. `price` is `None` when it could not be read.
pub fn estimate(usage: Usage, price: Option<Price>) -> Billing {
    match usage {
        Usage::Cached => Billing::free("cached"),
        Usage::NotRun => Billing::free("not_run_remotely"),
        Usage::Local => Billing::free("ran_locally"),
        Usage::Remote { seconds } => {
            let seconds = if seconds.is_finite() && seconds > 0.0 {
                seconds
            } else {
                0.0
            };
            match price {
                None => Billing {
                    credits_estimated: None,
                    billable_seconds: seconds,
                    free_reason: None,
                    estimate: true,
                    credits_per_10s: None,
                },
                Some(Price {
                    no_charge: true, ..
                })
                | Some(Price {
                    credits_per_10s: None,
                    ..
                }) => Billing {
                    credits_estimated: Some(0),
                    billable_seconds: seconds,
                    free_reason: Some("account_not_charged".to_string()),
                    estimate: true,
                    credits_per_10s: price.and_then(|price| price.credits_per_10s),
                },
                Some(Price {
                    credits_per_10s: Some(rate),
                    ..
                }) => Billing {
                    credits_estimated: Some(credits_for(seconds, rate)),
                    billable_seconds: seconds,
                    free_reason: None,
                    estimate: true,
                    credits_per_10s: Some(rate),
                },
            }
        }
    }
}

/// One line of the usage log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageLine {
    /// RFC 3339, UTC.
    pub time: String,
    pub tool: String,
    pub run_id: String,
    pub billable_seconds: f64,
    pub credits_estimated: Option<u64>,
    pub cached: bool,
    /// Whether the command ran remotely.
    pub ran: bool,
    /// A digest of the credential the call used, never the credential.
    pub key_fingerprint: String,
}

/// Append one line to the usage log. The file is created with owner-only permissions.
pub fn append_usage(path: &Path, line: &UsageLine) -> std::io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    let mut text = serde_json::to_string(line)?;
    text.push('\n');
    // One write per line, so lines from several processes do not interleave.
    file.write_all(text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u64 = 20;

    #[test]
    fn credits_round_up_per_check() {
        // 20 credits per 10 s is 1 credit per half second.
        assert_eq!(credits_for(0.0, RATE), 0);
        assert_eq!(credits_for(0.01, RATE), 1);
        assert_eq!(credits_for(0.5, RATE), 1);
        assert_eq!(credits_for(0.51, RATE), 2);
        assert_eq!(credits_for(2.0, RATE), 4);
        assert_eq!(credits_for(4.2, RATE), 9);
        assert_eq!(credits_for(10.0, RATE), 20);
        assert_eq!(credits_for(61.3, RATE), 123);
        assert_eq!(credits_for(-3.0, RATE), 0);
        assert_eq!(credits_for(f64::NAN, RATE), 0);
        assert_eq!(credits_for(5.0, 0), 0);
    }

    #[test]
    fn float_noise_does_not_add_a_credit() {
        assert_eq!(credits_for(0.1 + 0.2 + 0.2, RATE), 1);
        assert_eq!(credits_for(1.1 * 5.0, RATE), 11);
    }

    #[test]
    fn a_remote_run_is_charged_by_command_time() {
        let price = Price {
            credits_per_10s: Some(RATE),
            no_charge: false,
        };
        let billing = estimate(Usage::Remote { seconds: 4.2 }, Some(price));
        assert_eq!(billing.credits_estimated, Some(9));
        assert_eq!(billing.billable_seconds, 4.2);
        assert_eq!(billing.free_reason, None);
        assert!(billing.estimate);
        assert_eq!(billing.credits_per_10s, Some(RATE));
    }

    #[test]
    fn cached_turned_away_and_local_runs_are_free() {
        let price = Some(Price {
            credits_per_10s: Some(RATE),
            no_charge: false,
        });
        for (usage, reason) in [
            (Usage::Cached, "cached"),
            (Usage::NotRun, "not_run_remotely"),
            (Usage::Local, "ran_locally"),
        ] {
            let billing = estimate(usage, price);
            assert_eq!(billing.credits_estimated, Some(0));
            assert_eq!(billing.billable_seconds, 0.0);
            assert_eq!(billing.free_reason.as_deref(), Some(reason));
        }
    }

    #[test]
    fn an_account_that_is_not_charged_pays_nothing() {
        let billing = estimate(
            Usage::Remote { seconds: 30.0 },
            Some(Price {
                credits_per_10s: Some(RATE),
                no_charge: true,
            }),
        );
        assert_eq!(billing.credits_estimated, Some(0));
        assert_eq!(billing.billable_seconds, 30.0);
        assert_eq!(billing.free_reason.as_deref(), Some("account_not_charged"));
        let free = estimate(
            Usage::Remote { seconds: 30.0 },
            Some(Price {
                credits_per_10s: None,
                no_charge: false,
            }),
        );
        assert_eq!(free.credits_estimated, Some(0));
    }

    #[test]
    fn an_unknown_price_gives_no_figure() {
        let billing = estimate(Usage::Remote { seconds: 3.0 }, None);
        assert_eq!(billing.credits_estimated, None);
        assert_eq!(billing.billable_seconds, 3.0);
        assert_eq!(billing.free_reason, None);
    }

    #[test]
    fn usage_lines_are_appended_one_per_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs").join("usage.jsonl");
        for index in 0..3 {
            append_usage(
                &path,
                &UsageLine {
                    time: "2026-01-01T00:00:00Z".into(),
                    tool: "check".into(),
                    run_id: format!("20260101-00000{index}-aaaa"),
                    billable_seconds: 1.5,
                    credits_estimated: Some(3),
                    cached: false,
                    ran: true,
                    key_fingerprint: "0123456789ab".into(),
                },
            )
            .unwrap();
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<UsageLine> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[2].run_id, "20260101-000002-aaaa");
        assert_eq!(lines[0].credits_estimated, Some(3));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }
}
