//! Boundary-finding utilities for Titan venues (vendored from the template).
//!
//! Binary-searches a venue's `quote()` for the maximal valid input range for a
//! token pair. A quote is valid when it returns `Ok`, `not_enough_liquidity ==
//! false`, and `expected_output > 0`.

use crate::trading_venue::{error::TradingVenueError, QuoteResult};

const SCALING_FACTOR: u64 = 2;

fn valid_quote(quote: &QuoteResult) -> bool {
    !(quote.not_enough_liquidity || quote.expected_output == 0)
}

/// Coarse exponential search returning `(lower_low, lower_high, upper_low, upper_high)`.
pub fn find_boundaries_coarse(f: &impl Fn(u64) -> Result<QuoteResult, TradingVenueError>) -> Result<(u64, u64, u64, u64), TradingVenueError> {
    // Phase 1: find first valid quote.
    let mut lower_low = 0;
    let mut lower_high = 1;

    while {
        match f(lower_high) {
            Ok(result) if !valid_quote(&result) => true,
            Ok(_result) => false,
            Err(_) => true,
        }
    } {
        lower_low = lower_high;
        lower_high = lower_high.saturating_mul(SCALING_FACTOR);

        if lower_high <= lower_low || lower_high == u64::MAX {
            log::error!("Invalid lower/upper combination or hit u64::MAX");
            lower_high = u64::MAX;
            break;
        }
    }

    // Phase 2: find first invalid quote above the valid region.
    let mut upper_low = lower_high;
    let mut upper_high = upper_low.saturating_mul(SCALING_FACTOR);

    if upper_high <= upper_low {
        upper_high = upper_low;
    } else {
        while let Ok(result) = f(upper_high) {
            if !valid_quote(&result) {
                break;
            }

            upper_low = upper_high;
            upper_high = upper_high.saturating_mul(SCALING_FACTOR);

            if upper_high <= upper_low || upper_high == u64::MAX {
                log::trace!("Hit overflow during upper search");
                upper_high = u64::MAX;
                break;
            }
        }
    }

    Ok((lower_low, lower_high, upper_low, upper_high))
}

/// Binary-refine the lower boundary (smallest valid input).
pub fn refine_lower(f: &impl Fn(u64) -> Result<QuoteResult, TradingVenueError>, mut low: u64, mut high: u64) -> Result<u64, TradingVenueError> {
    let low_quote = f(low);
    let high_quote = f(high);

    if let Ok(ref result) = low_quote {
        if valid_quote(result) {
            log::error!("The lower low quotes successfully; this contradicts the search invariant.");
        }
    }

    match high_quote {
        Ok(result) => {
            if !valid_quote(&result) {
                log::error!("The upper low is invalid; this contradicts the search invariant.");
            }
        },
        Err(e) => {
            log::error!("The upper low errored; this contradicts the search invariant: {:?}", e);
        },
    }

    while (high - low) > 100 {
        let mid = high / 2 + low / 2;

        match f(mid) {
            Ok(result) => {
                if valid_quote(&result) {
                    high = mid;
                } else {
                    low = mid;
                }
            },
            Err(_) => low = mid,
        }
    }

    Ok(high)
}

/// Binary-refine the upper boundary (largest valid input).
pub fn refine_upper(f: &impl Fn(u64) -> Result<QuoteResult, TradingVenueError>, mut low: u64, mut high: u64) -> Result<u64, TradingVenueError> {
    let low_quote = f(low);
    let high_quote = f(high);

    match low_quote {
        Ok(result) => {
            if !valid_quote(&result) {
                log::error!("The upper low is invalid; this contradicts invariants.");
            }
        },
        Err(e) => {
            log::error!("The upper low errored; this contradicts invariants: {:?}", e);
        },
    }

    if let Ok(ref result) = high_quote {
        if valid_quote(result) && high != u64::MAX {
            log::error!("The upper high is valid; this contradicts the expected invalid boundary.");
        }
    }

    while (high - low) > 100 {
        let mid = high / 2 + low / 2;

        match f(mid) {
            Ok(result) => {
                if valid_quote(&result) {
                    low = mid;
                } else {
                    high = mid;
                }
            },
            Err(_) => high = mid,
        }
    }

    Ok(low)
}

/// Unified boundary search returning `(lower_bound, upper_bound)`.
pub fn find_boundaries(f: &impl Fn(u64) -> Result<QuoteResult, TradingVenueError>) -> Result<(u64, u64), TradingVenueError> {
    let (lower_low, lower_high, upper_low, upper_high) = find_boundaries_coarse(f)?;

    if lower_low == upper_high {
        return Err(TradingVenueError::BoundarySearchFailed("Search boundaries are all equal; search space collapsed".into()));
    }

    if lower_high == u64::MAX {
        return Err(TradingVenueError::NoQuotableValue("No quotable value found; exponential search hit u64::MAX".into()));
    }

    let lower_bound = refine_lower(f, lower_low, lower_high)?;
    let upper_bound = refine_upper(f, upper_low, upper_high)?;

    Ok((lower_bound, upper_bound))
}
