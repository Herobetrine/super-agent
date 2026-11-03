//! Candidate selection for operator-enabled provider rotation.
//!
//! Round-robin: start right after the failing route's position in the
//! operator's order (or at the head when the failing route is not listed),
//! wrap once, and pick the first candidate that is not the failing route, not
//! the failing provider when the failure is credential-level (auth/billing),
//! and not blocked by its circuit breaker.

use crate::adapter::{ErrorClass, Route};

/// Same `(provider, model)`; the mode is not part of route identity here.
pub fn same_route(a: &Route, b: &Route) -> bool {
    a.provider == b.provider && a.model == b.model
}

/// Index of the next eligible candidate. `blocked(route)` reports whether the
/// route's breaker currently refuses admission.
pub fn next_candidate(
    order: &[Route],
    failing: &Route,
    class: ErrorClass,
    mut blocked: impl FnMut(&Route) -> bool,
) -> Option<usize> {
    let n = order.len();
    if n == 0 {
        return None;
    }
    let start = order
        .iter()
        .position(|r| same_route(r, failing))
        .map_or(0, |i| i + 1);
    let provider_level = matches!(class, ErrorClass::Auth | ErrorClass::Billing);
    (0..n).map(|k| (start + k) % n).find(|&i| {
        let r = &order[i];
        !same_route(r, failing) && !(provider_level && r.provider == failing.provider) && !blocked(r)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(p: &str, m: &str) -> Route {
        Route {
            provider: p.into(),
            model: m.into(),
            mode: "cli".into(),
        }
    }

    #[test]
    fn round_robin_after_current() {
        let order = [r("a", "1"), r("b", "1"), r("c", "1")];
        assert_eq!(next_candidate(&order, &r("b", "1"), ErrorClass::Network, |_| false), Some(2));
        assert_eq!(next_candidate(&order, &r("c", "1"), ErrorClass::Network, |_| false), Some(0));
        assert_eq!(next_candidate(&order, &r("x", "1"), ErrorClass::Network, |_| false), Some(0));
        assert_eq!(
            next_candidate(&order, &r("a", "1"), ErrorClass::Network, |c| c.provider == "b"),
            Some(2)
        );
        assert_eq!(next_candidate(&order, &r("a", "1"), ErrorClass::Network, |_| true), None);
        assert_eq!(next_candidate(&[r("a", "1")], &r("a", "1"), ErrorClass::Network, |_| false), None);
    }

    #[test]
    fn credential_failure_skips_same_provider() {
        let order = [r("a", "1"), r("a", "2"), r("b", "1")];
        assert_eq!(next_candidate(&order, &r("a", "1"), ErrorClass::Auth, |_| false), Some(2));
        assert_eq!(next_candidate(&order, &r("a", "1"), ErrorClass::Overloaded, |_| false), Some(1));
    }
}
